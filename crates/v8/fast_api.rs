//! The fast-call type descriptions (v8::fast_api).
//!
//! A fast call is a C function — one the host compiled, or one a macro like
//! Deno's `#[op2(fast)]` emits — plus a description of its signature, which the
//! engine uses to decide whether a call can skip the ordinary argument
//! marshalling. [`CFunction`] holds the address and the description;
//! `FunctionBuilder::build_fast` is what registers them.
//!
//! # What the bridge does with them
//!
//! It describes them faithfully and does **not** take the fast path: a function
//! built with fast overloads is called through the ordinary callback, with the
//! same arguments and the same result. V8 itself falls back to the slow path
//! whenever a call's arguments do not match the fast signature, so the
//! behaviour is the documented one for that case; the performance is not.
//! Nothing here is handed to a C++ runtime, because there is none to hand it to:
//! [`CFunction::address`] is what would be called, and the bridge does not call
//! it.

use std::ffi::c_void;

use crate::Isolate;
use crate::data::Value;
use crate::handle::Local;

/// A fast call's address and type information (v8::fast_api::CFunction).
#[derive(Clone, Copy)]
#[repr(C)]
pub struct CFunction {
    address: *const c_void,
    type_info: *const CFunctionInfo,
}

impl CFunction {
    /// Construct a `CFunction` from a function address and its type info.
    ///
    /// `type_info` is borrowed for `'static` because the resulting value stores
    /// its address, exactly as the crate we stand in for does: in practice the
    /// caller builds it inside a `const` initializer, where a reference to a
    /// temporary is promoted to `'static`.
    pub const fn new(address: *const c_void, type_info: &'static CFunctionInfo) -> Self {
        Self { address, type_info }
    }

    /// The address of the function a fast call would enter.
    pub const fn address(&self) -> *const c_void {
        self.address
    }

    /// The signature the address is expected to have.
    pub const fn type_info(&self) -> &CFunctionInfo {
        // SAFETY: `new` stores the address of a reference that outlives this
        // value, so the pointer is still valid here.
        unsafe { &*self.type_info }
    }
}

/// A fast call's signature (v8::fast_api::CFunctionInfo).
///
/// `arg_info` is borrowed for `'static` for the same reason `CFunction`'s type
/// info is: the description outlives every function built with it.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct CFunctionInfo {
    arg_count: i32,
    arg_info: *const CTypeInfo,
    int64_representation: Int64Representation,
    return_info: CTypeInfo,
}

impl CFunctionInfo {
    /// Describe a fast call: what it returns, what it takes, and how it spells
    /// a 64-bit integer.
    ///
    /// Only the last argument may be [`Type::CallbackOptions`].
    pub const fn new(
        return_info: CTypeInfo,
        arg_info: &'static [CTypeInfo],
        repr: Int64Representation,
    ) -> Self {
        Self {
            arg_count: arg_info.len() as i32,
            arg_info: arg_info.as_ptr(),
            int64_representation: repr,
            return_info,
        }
    }
}

/// How a fast call's 64-bit integers are spelled (v8::fast_api::Int64Representation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Int64Representation {
    /// Numbers, as `9_007_199_254_740_991` is.
    Number = 0,
    /// BigInts, which do not lose precision.
    BigInt = 1,
}

/// One argument's or a return value's type (v8::fast_api::CTypeInfo).
#[derive(Clone, Copy)]
#[repr(C)]
pub struct CTypeInfo {
    r#type: Type,
    flags: u8,
}

impl CTypeInfo {
    pub const fn new(r#type: Type, flags: Flags) -> Self {
        Self {
            r#type,
            flags: flags.bits(),
        }
    }
}

/// The types a fast signature can name (v8::fast_api::Type).
///
/// The discriminants are V8's, so a description built here means the same thing
/// it would there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Type {
    Void = 0,
    Bool = 1,
    Uint8 = 2,
    Int32 = 3,
    Uint32 = 4,
    Int64 = 5,
    Uint64 = 6,
    Float32 = 7,
    Float64 = 8,
    Pointer = 9,
    V8Value = 10,
    SeqOneByteString = 11,
    ApiObject = 12,
    Any = 13,
    /// Outside the enum in V8 too: `CTypeInfo::kCallbackOptionsType`.
    CallbackOptions = 255,
}

impl Type {
    /// This type with no flags, which is how a signature names it.
    pub const fn as_info(self) -> CTypeInfo {
        CTypeInfo::new(self, Flags::empty())
    }
}

impl From<Type> for CTypeInfo {
    fn from(r#type: Type) -> Self {
        CTypeInfo::new(r#type, Flags::empty())
    }
}

bitflags::bitflags! {
    /// What a fast signature says about an argument
    /// (v8::fast_api::Flags).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Flags: u8 {
        /// Must be an ArrayBuffer or TypedArray.
        const AllowShared = 1 << 0;
        /// T must be integral.
        const EnforceRange = 1 << 1;
        /// T must be integral.
        const Clamp = 1 << 2;
        /// T must be float or double.
        const IsRestricted = 1 << 3;
    }
}

/// What a fast callback receives besides its arguments
/// (v8::fast_api::FastApiCallbackOptions).
#[repr(C)]
pub struct FastApiCallbackOptions<'a> {
    pub(crate) isolate: Isolate,
    /// The `data` the function was built with, or `undefined`.
    pub data: Local<'a, Value>,
}

impl<'a> FastApiCallbackOptions<'a> {
    /// The isolate the fast call runs on.
    ///
    /// # Safety
    ///
    /// The caller must be inside the fast call: the isolate is only guaranteed
    /// to be live for its duration.
    pub unsafe fn isolate_unchecked(&self) -> &'a Isolate {
        // SAFETY: `&self` is inside the options the fast call received, and the
        // `'a` asked for is the call's lifetime.
        unsafe { &*(&self.isolate as *const Isolate) }
    }

    /// The isolate the fast call runs on, mutably.
    ///
    /// # Safety
    ///
    /// As [`isolate_unchecked`](Self::isolate_unchecked), and the caller must
    /// not hand out a second mutable borrow.
    pub unsafe fn isolate_unchecked_mut(&mut self) -> &mut Isolate {
        &mut self.isolate
    }
}

/// A one-byte string a fast call receives
/// (v8::fast_api::FastApiOneByteString).
#[repr(C)]
pub struct FastApiOneByteString {
    data: *const u8,
    length: u32,
}

impl FastApiOneByteString {
    /// The string's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        if self.data.is_null() {
            // A null pointer is not a valid slice, not even an empty one.
            return &[];
        }
        // SAFETY: the fast-call ABI guarantees the bytes are live for the
        // length it reports.
        unsafe { std::slice::from_raw_parts(self.data, self.length as usize) }
    }
}
