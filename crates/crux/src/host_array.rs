//! Host-defined Array exotic objects (Ladybird's `JS_HOST_CLASS_ARRAY`): an
//! Array whose three structural internal methods a host answers.
//!
//! Only the operations in [`HostArrayOps`] may be replaced; every other
//! internal method is the Array exotic's own, which is what makes the object
//! still array-like (`length`, index properties, iteration, the `Array.prototype`
//! builtins). A method that returns `None` falls back to the Array internal
//! method, and [`JsObject::host_array_delegate_define_own_property`] and its
//! siblings run those methods directly for a host that adds to the array rather
//! than replacing its behaviour — Ladybird's
//! `js_host_array_array_{define_own_property,set,delete}`.
//!
//! [`JsObject::host_array_delegate_define_own_property`]: crate::object::JsObject::host_array_delegate_define_own_property

use crate::error::JsError;
use crate::object::JsObject;
use crate::property::{PropertyDescriptor, PropertyKey};
use crate::value::Value;

/// The Array internal methods a host may answer, and only these. Each returns
/// `Option<Result<..>>`: `None` falls back to the Array internal method,
/// `Some(Err(..))` surfaces an error, `Some(Ok(..))` overrides.
pub trait HostArrayOps: std::fmt::Debug {
    /// [[DefineOwnProperty]] (spec 10.4.2.1): `length` and index properties.
    fn define_property(
        &self,
        _array: &JsObject,
        _key: &PropertyKey,
        _desc: &PropertyDescriptor,
    ) -> Option<Result<bool, JsError>> {
        None
    }

    /// [[Set]] (P, V, Receiver, Throw).
    fn set(
        &self,
        _array: &JsObject,
        _key: &PropertyKey,
        _value: &Value,
        _receiver: &Value,
        _throw: bool,
    ) -> Option<Result<bool, JsError>> {
        None
    }

    /// [[Delete]] (P).
    fn delete(&self, _array: &JsObject, _key: &PropertyKey) -> Option<Result<bool, JsError>> {
        None
    }
}
