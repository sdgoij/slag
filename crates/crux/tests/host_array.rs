//! Ladybird's `JS_HOST_CLASS_ARRAY` against Slag's Array exotic: the three
//! structural internal methods belong to the host, every other one to the Array.

use std::cell::Cell;
use std::rc::Rc;

use crux::error::JsError;
use crux::host_array::HostArrayOps;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::value::Value;

/// Counts the structural operations it is asked about and delegates each one, so
/// the array really changes.
#[derive(Debug, Default)]
struct Delegate {
    defined: Cell<u32>,
    set: Cell<u32>,
    deleted: Cell<u32>,
}

impl HostArrayOps for Delegate {
    fn define_property(
        &self,
        array: &JsObject,
        key: &PropertyKey,
        desc: &PropertyDescriptor,
    ) -> Option<Result<bool, JsError>> {
        self.defined.set(self.defined.get() + 1);
        Some(array.host_array_delegate_define_own_property(key, desc))
    }

    fn set(
        &self,
        array: &JsObject,
        key: &PropertyKey,
        value: &Value,
        receiver: &Value,
        throw: bool,
    ) -> Option<Result<bool, JsError>> {
        self.set.set(self.set.get() + 1);
        Some(array.host_array_delegate_set(key, *value, *receiver, throw))
    }

    fn delete(&self, array: &JsObject, key: &PropertyKey) -> Option<Result<bool, JsError>> {
        self.deleted.set(self.deleted.get() + 1);
        Some(array.host_array_delegate_delete(key))
    }
}

/// Refuses every structural operation.
#[derive(Debug)]
struct Refuse;

impl HostArrayOps for Refuse {
    fn define_property(
        &self,
        _array: &JsObject,
        _key: &PropertyKey,
        _desc: &PropertyDescriptor,
    ) -> Option<Result<bool, JsError>> {
        Some(Ok(false))
    }

    fn delete(&self, _array: &JsObject, _key: &PropertyKey) -> Option<Result<bool, JsError>> {
        Some(Ok(false))
    }
}

fn read(array: &JsObject, key: &str) -> Value {
    array
        .get_key(&PropertyKey::from_utf8(key))
        .expect("the property read")
}

#[test]
fn a_host_array_hook_answers_the_three_structural_operations() {
    let ops = Rc::new(Delegate::default());
    let array = JsObject::host_array_create(None, ops.clone());
    assert_eq!(read(&array, "length"), Value::Number(0.0));
    assert_eq!(
        array.array_length_dense(),
        None,
        "a host array is in the generic element form, so no dense path can bypass the hooks"
    );

    let receiver = array.self_value();
    array
        .set_with_receiver_key(
            &PropertyKey::from_utf8("0"),
            Value::Number(7.0),
            receiver,
            true,
        )
        .expect("the element write");
    assert_eq!(ops.set.get(), 1, "the [[Set]] hook ran");
    assert_eq!(
        read(&array, "0"),
        Value::Number(7.0),
        "the delegated set stored the element"
    );
    assert_eq!(
        read(&array, "length"),
        Value::Number(1.0),
        "the delegated define kept length one past the index"
    );

    // The delegated set reaches the define hook too (OrdinarySet ends in
    // [[DefineOwnProperty]]), which is exactly Ladybird's nesting.
    let defined = ops.defined.get();
    array
        .define_property_key(
            &PropertyKey::from_utf8("1"),
            &PropertyDescriptor::data(Value::Number(8.0)),
        )
        .expect("the define");
    assert_eq!(ops.defined.get(), defined + 1, "the define hook ran");
    assert_eq!(read(&array, "length"), Value::Number(2.0));

    assert!(
        array
            .delete_key(&PropertyKey::from_utf8("0"))
            .expect("the delete"),
        "the delegated delete succeeds"
    );
    assert_eq!(ops.deleted.get(), 1, "the [[Delete]] hook ran");
    assert_eq!(read(&array, "0"), Value::Undefined);
}

#[test]
fn a_host_array_hook_can_refuse_a_define_and_a_delete() {
    let array = JsObject::host_array_create(None, Rc::new(Refuse));
    assert!(
        !array
            .define_property_key(
                &PropertyKey::from_utf8("0"),
                &PropertyDescriptor::data(Value::Number(1.0)),
            )
            .expect("the refused define"),
        "a hook's false is the define's result"
    );
    assert_eq!(read(&array, "0"), Value::Undefined, "nothing was stored");
    assert_eq!(
        read(&array, "length"),
        Value::Number(0.0),
        "length is unchanged"
    );
    assert!(
        !array
            .delete_key(&PropertyKey::from_utf8("length"))
            .expect("the refused delete"),
        "a hook's false is the delete's result"
    );
}
