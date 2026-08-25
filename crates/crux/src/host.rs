//! Host-defined exotic objects (the `ObjectKind::Host` variant): internal
//! methods dispatch to a [`HostOps`] implementation with ordinary fallback.
//!
//! This is the seam the C drop-in surfaces use for host objects: the JSC
//! `JSClassRef` callbacks (crates/jsc) and V8 handler objects (crates/v8)
//! implement `HostOps`, and every method defaults to `None` — not
//! intercepted, so the ordinary internal method runs (the model of e.g.
//! JavaScriptCore's `JSClassRef` callbacks, where an absent callback
//! behaves ordinarily).

use crate::error::JsError;
use crate::object::{JsObject, Property};
use crate::property::{PropertyDescriptor, PropertyKey};
use crate::value::Value;

/// The host-defined behaviour of an `ObjectKind::Host` object. Each method
/// returns `Option<Result<..>>`: `None` falls back to the ordinary internal
/// method, `Some(Err(..))` surfaces an error, `Some(Ok(..))` overrides.
pub trait HostOps: std::fmt::Debug {
    /// [[GetOwnProperty]].
    fn get_own_property(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
    ) -> Option<Result<Property, JsError>> {
        None
    }

    /// [[DefineOwnProperty]].
    fn define_property(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
        _desc: &PropertyDescriptor,
    ) -> Option<Result<bool, JsError>> {
        None
    }

    /// [[HasProperty]] (the prototype-walking form, not [[HasOwnProperty]]).
    fn has_property(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
    ) -> Option<Result<bool, JsError>> {
        None
    }

    /// [[Get]] (P, Receiver).
    fn get(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
        _receiver: &Value,
    ) -> Option<Result<Value, JsError>> {
        None
    }

    /// [[Set]] (P, V, Receiver).
    fn set(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
        _value: &Value,
        _receiver: &Value,
    ) -> Option<Result<bool, JsError>> {
        None
    }

    /// [[Delete]].
    fn delete(&self, _object: &JsObject, _key: &PropertyKey) -> Option<Result<bool, JsError>> {
        None
    }

    /// [[OwnPropertyKeys]].
    fn own_property_keys(&self, _object: &JsObject) -> Option<Result<Vec<PropertyKey>, JsError>> {
        None
    }

    /// [[Call]]: a host object that reports [`is_callable`](Self::is_callable)
    /// must implement this.
    fn call(
        &self,
        _object: &JsObject,
        _this: &Value,
        _args: &[Value],
    ) -> Option<Result<Value, JsError>> {
        None
    }

    /// [[Construct]]: a host object that reports
    /// [`is_constructible`](Self::is_constructible) must implement this.
    fn construct(
        &self,
        _object: &JsObject,
        _args: &[Value],
        _new_target: &Value,
    ) -> Option<Result<Value, JsError>> {
        None
    }

    /// Whether `typeof` reports the object as `"function"` (and `call` is
    /// implemented).
    fn is_callable(&self) -> bool {
        false
    }

    /// Whether the object is a constructor (and `construct` is implemented).
    fn is_constructible(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::JsObject;
    use crate::string::JsString;

    /// A host object that intercepts reads of `magic` and behaves
    /// ordinarily otherwise.
    #[derive(Debug)]
    struct InterceptGet;

    impl HostOps for InterceptGet {
        fn get(
            &self,
            _object: &JsObject,
            key: &PropertyKey,
            _receiver: &Value,
        ) -> Option<Result<Value, JsError>> {
            if key == &PropertyKey::from_utf8("magic") {
                Some(Ok(Value::Number(42.0)))
            } else {
                None
            }
        }
    }

    #[test]
    fn host_get_intercepts_magic_and_falls_back_otherwise() {
        let object = JsObject::host_object_create(std::rc::Rc::new(InterceptGet), None);
        assert_eq!(
            object.get(&JsString::from_utf8("magic")).unwrap(),
            Value::Number(42.0)
        );
        // Unintercepted keys fall through to ordinary (absent -> undefined).
        assert_eq!(
            object.get(&JsString::from_utf8("other")).unwrap(),
            Value::Undefined
        );
    }

    /// A callable host object.
    #[derive(Debug)]
    struct Callable;

    impl HostOps for Callable {
        fn is_callable(&self) -> bool {
            true
        }

        fn call(
            &self,
            _object: &JsObject,
            _this: &Value,
            args: &[Value],
        ) -> Option<Result<Value, JsError>> {
            Some(Ok(args.first().cloned().unwrap_or(Value::Undefined)))
        }
    }

    #[test]
    fn callable_host_objects_report_function_and_call() {
        let object = JsObject::host_object_create(std::rc::Rc::new(Callable), None);
        let value = Value::Object(object);
        assert!(crate::value::is_callable(&value));
        assert_eq!(crate::value::type_of(&value), "function");
        let result =
            crate::function::call(&value, Value::Undefined, &[Value::Number(7.0)]).unwrap();
        assert_eq!(result, Value::Number(7.0));
    }
}
