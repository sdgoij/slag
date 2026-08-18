//! A drop-in implementation of the JavaScriptCore C API (the
//! `JSContextRef` family) backed by Slag.
//!
//! C/C++ programs written against the documented JavaScriptCore C API —
//! `#include <JavaScriptCore/JSContextRef.h>` and friends — compile against
//! the headers in `include/JavaScriptCore/` and link against this crate's
//! `libslag_jsc` (static or shared) without source changes, as long as they
//! stick to the documented public surface.
//!
//! Opaque refs are encoded ids: objects (and function values) use the
//! object's own stable id (tagged with the low bit, so `==` on refs is
//! object identity), primitive values and strings are ids into strongly
//! owned thread-local tables (`ffi`). Values handed to the host never
//! dangle: they are held for as long as the ref exists (or the context is
//! torn down). `JSValueUnprotect` frees primitive refs early; objects live
//! as long as the JS graph keeps them (like JSC before a collection).
//!
//! Divergences from JSC, all documented in the headers:
//! - one context per isolate; `JSContextGroupCreate` is accepted but groups
//!   do not share a heap
//! - typed-array APIs are not implemented yet (compile-time absent)
//! - `finalize` runs when the object's last strong reference drops, so
//!   cyclic JS graphs never finalize

// The exported functions must keep their exact C names, and their safety
// contract (valid refs, live context) is the documented C API's.
#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

mod class;
mod context;
mod object;
mod refs;
mod string;
mod value;

// The exported C surface: every `extern "C"` function is re-exported at
// the crate root so it is reachable (and therefore both exported from the
// cdylib and never flagged as dead code).
pub use class::{JSClassCreate, JSClassRelease, JSClassRetain};
pub use context::{
    JSCheckScriptSyntax, JSContextCreateBacktrace, JSContextGetException, JSContextGetGlobalObject,
    JSContextGetGroup, JSContextGroupCreate, JSContextGroupRelease, JSContextGroupRetain,
    JSContextSetException, JSEvaluateScript, JSGlobalContextCreate, JSGlobalContextCreateInGroup,
    JSGlobalContextRelease, JSGlobalContextRetain,
};
pub use object::{
    JSObjectCallAsConstructor, JSObjectCallAsFunction, JSObjectCopyPropertyNames,
    JSObjectDeleteProperty, JSObjectGetPrivate, JSObjectGetProperty, JSObjectGetPropertyAtIndex,
    JSObjectGetPrototype, JSObjectHasProperty, JSObjectIsConstructor, JSObjectIsFunction,
    JSObjectMake, JSObjectMakeArray, JSObjectMakeDate, JSObjectMakeError, JSObjectMakeFunction,
    JSObjectMakeFunctionWithCallback, JSObjectMakeRegExp, JSObjectSetPrivate, JSObjectSetProperty,
    JSObjectSetPropertyAtIndex, JSObjectSetPrototype, JSPropertyNameAccumulatorAddName,
    JSPropertyNameArrayGetCount, JSPropertyNameArrayGetNameAtIndex, JSPropertyNameArrayRelease,
    JSPropertyNameArrayRetain,
};
pub use string::{
    JSStringCreateWithCharacters, JSStringCreateWithUTF8CString, JSStringGetCharactersPtr,
    JSStringGetLength, JSStringGetMaximumUTF8CStringSize, JSStringGetUTF8CString, JSStringIsEqual,
    JSStringIsEqualToUTF8CString, JSStringRelease, JSStringRetain,
};
pub use value::{
    JSValueCreateJSONString, JSValueGetType, JSValueIsArray, JSValueIsBoolean, JSValueIsDate,
    JSValueIsEqual, JSValueIsInstanceOfConstructor, JSValueIsNull, JSValueIsNumber,
    JSValueIsObject, JSValueIsObjectOfClass, JSValueIsStrictEqual, JSValueIsString,
    JSValueIsSymbol, JSValueIsUndefined, JSValueMakeBoolean, JSValueMakeFromJSONString,
    JSValueMakeNull, JSValueMakeNumber, JSValueMakeString, JSValueMakeSymbol, JSValueMakeUndefined,
    JSValueProtect, JSValueToBoolean, JSValueToNumber, JSValueToObject, JSValueToStringCopy,
    JSValueUnprotect,
};

/// Opaque handle types; never dereferenced (refs are encoded ids).
#[repr(C)]
pub struct OpaqueJSContext {
    _private: [u8; 0],
}
#[repr(C)]
pub struct OpaqueJSContextGroup {
    _private: [u8; 0],
}
#[repr(C)]
pub struct OpaqueJSValue {
    _private: [u8; 0],
}
#[repr(C)]
pub struct OpaqueJSString {
    _private: [u8; 0],
}
#[repr(C)]
pub struct OpaqueJSClass {
    _private: [u8; 0],
}
#[repr(C)]
pub struct OpaqueJSPropertyNameArray {
    _private: [u8; 0],
}
#[repr(C)]
pub struct OpaqueJSPropertyNameAccumulator {
    _private: [u8; 0],
}

pub type JSContextRef = *mut OpaqueJSContext;
pub type JSGlobalContextRef = *mut OpaqueJSContext;
pub type JSContextGroupRef = *mut OpaqueJSContextGroup;
pub type JSValueRef = *mut OpaqueJSValue;
pub type JSObjectRef = *mut OpaqueJSValue;
pub type JSStringRef = *mut OpaqueJSString;
pub type JSClassRef = *mut OpaqueJSClass;
pub type JSPropertyNameArrayRef = *mut OpaqueJSPropertyNameArray;
pub type JSPropertyNameAccumulatorRef = *mut OpaqueJSPropertyNameAccumulator;

/// A Unicode character (a UTF-16 code unit).
pub type JSChar = u16;

/// The type of a JS value (JSValueGetType).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JSType {
    #[default]
    Undefined = 0,
    Null = 1,
    Boolean = 2,
    Number = 3,
    String = 4,
    Object = 5,
    Symbol = 6,
    BigInt = 7,
}

/// The C `JSClassDefinition` layout (JSClassRef.h).
#[repr(C)]
#[derive(Debug, Clone)]
pub struct JSClassDefinition {
    pub version: i32,
    pub attributes: u32,
    pub class_name: *const std::ffi::c_char,
    pub parent_class: JSClassRef,
    pub static_values: *const JSStaticValue,
    pub static_functions: *const JSStaticFunction,
    pub initialize: Option<JSObjectInitializeCallback>,
    pub finalize: Option<JSObjectFinalizeCallback>,
    pub has_property: Option<JSObjectHasPropertyCallback>,
    pub get_property: Option<JSObjectGetPropertyCallback>,
    pub set_property: Option<JSObjectSetPropertyCallback>,
    pub delete_property: Option<JSObjectDeletePropertyCallback>,
    pub get_property_names: Option<JSObjectGetPropertyNamesCallback>,
    pub call_as_function: Option<JSObjectCallAsFunctionCallback>,
    pub call_as_constructor: Option<JSObjectCallAsConstructorCallback>,
    pub has_instance: Option<JSObjectHasInstanceCallback>,
    pub convert_to_type: Option<JSObjectConvertToTypeCallback>,
}

/// The C `JSStaticValue` layout.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct JSStaticValue {
    pub name: *const std::ffi::c_char,
    pub get_property: Option<JSObjectGetPropertyCallback>,
    pub set_property: Option<JSObjectSetPropertyCallback>,
    pub attributes: u32,
}

/// The C `JSStaticFunction` layout.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct JSStaticFunction {
    pub name: *const std::ffi::c_char,
    pub call_as_function: Option<JSObjectCallAsFunctionCallback>,
    pub attributes: u32,
}

pub type JSObjectInitializeCallback = unsafe extern "C" fn(JSContextRef, JSObjectRef);
pub type JSObjectFinalizeCallback = unsafe extern "C" fn(JSObjectRef);
pub type JSObjectHasPropertyCallback =
    unsafe extern "C" fn(JSContextRef, JSObjectRef, JSStringRef) -> bool;
pub type JSObjectGetPropertyCallback =
    unsafe extern "C" fn(JSContextRef, JSObjectRef, JSStringRef, *mut JSValueRef) -> JSValueRef;
pub type JSObjectSetPropertyCallback = unsafe extern "C" fn(
    JSContextRef,
    JSObjectRef,
    JSStringRef,
    JSValueRef,
    *mut JSValueRef,
) -> bool;
pub type JSObjectDeletePropertyCallback =
    unsafe extern "C" fn(JSContextRef, JSObjectRef, JSStringRef, *mut JSValueRef) -> bool;
pub type JSObjectGetPropertyNamesCallback =
    unsafe extern "C" fn(JSContextRef, JSObjectRef, JSPropertyNameAccumulatorRef);
pub type JSObjectCallAsFunctionCallback = unsafe extern "C" fn(
    JSContextRef,
    JSObjectRef,
    JSObjectRef,
    usize,
    *const JSValueRef,
    *mut JSValueRef,
) -> JSValueRef;
pub type JSObjectCallAsConstructorCallback = unsafe extern "C" fn(
    JSContextRef,
    JSObjectRef,
    usize,
    *const JSValueRef,
    *mut JSValueRef,
) -> JSObjectRef;
pub type JSObjectHasInstanceCallback =
    unsafe extern "C" fn(JSContextRef, JSObjectRef, JSValueRef, *mut JSValueRef) -> bool;
pub type JSObjectConvertToTypeCallback =
    unsafe extern "C" fn(JSContextRef, JSObjectRef, JSType, *mut JSValueRef) -> JSValueRef;

pub use ffi::guard;
