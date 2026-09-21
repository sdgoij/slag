//! Primitive arrays (`v8::PrimitiveArray`): the small fixed-size array of
//! primitives a host attaches to a script as host-defined options.
//!
//! The crate we stand in for has a C++ heap object here. The engine has none, so
//! what a host writes is the JavaScript array the bridge built for it — the same
//! decision [`FixedArray`](crate::FixedArray) records, and for the same reason:
//! the shape a host uses is a length plus integer-indexed reads and writes.
//!
//! One consequence is worth knowing before it is met: a `PrimitiveArray` here
//! *is* an array, so script that were handed one could see it, where the crate's
//! is a heap object no script reaches. Nothing in this bridge hands one to
//! script, and the value is invisible to the walks that matter because it is
//! never a property of anything.

use runtime::api;

use crate::data::{Array, Primitive, PrimitiveArray, Value};
use crate::handle::Local;
use crate::scope::PinScope;

impl PrimitiveArray {
    /// A new array of `length` slots, each reading as *undefined* until a host
    /// writes one (v8::PrimitiveArray::New).
    ///
    /// The engine's array constructor takes elements rather than a length, so a
    /// slot nothing wrote is *undefined* here where the crate's is an
    /// uninitialized slot. The difference is not observable through the shape: a
    /// read of an unwritten slot answers *undefined* there too, and nothing else
    /// can see the slots.
    pub fn new<'s>(scope: &PinScope<'s, '_>, length: usize) -> Local<'s, PrimitiveArray> {
        let realm = crate::realm_of(scope);
        let slots = vec![api::Local::undefined(); length];
        match api::Array::new(&realm, &slots) {
            Ok(array) => {
                let array: Local<'_, Array> = Local::from_engine(array);
                array.retag()
            }
            Err(error) => {
                crate::throw(scope, &error);
                panic!("bridge: creating a primitive array failed: {error}");
            }
        }
    }
}

impl<'s> Local<'s, PrimitiveArray> {
    /// The number of slots (v8::PrimitiveArray::Length).
    pub fn length(&self) -> usize {
        let realm = crate::realm_current();
        api::Array::length(&realm, self.engine()).map_or(0, |length| length as usize)
    }

    /// Write `item` into slot `index` (v8::PrimitiveArray::Set).
    ///
    /// Nothing comes back, as there: the crate's signature is `()`, so a write
    /// the engine refuses becomes the isolate's pending exception — the same
    /// place a throw from the crate's own `set` ends up.
    pub fn set(&self, scope: &PinScope<'_, '_>, index: usize, item: Local<'_, Primitive>) {
        let realm = crate::realm_of(scope);
        if let Err(error) = api::Array::set(&realm, self.engine(), index as u32, item.engine()) {
            crate::throw(scope, &error);
        }
    }

    /// Read the slot at `index` (v8::PrimitiveArray::Get).
    ///
    /// The value is retagged rather than checked for being a primitive: the
    /// slots hold what [`set`](Self::set) wrote and what [`new`](Self::new)
    /// filled, so a primitive is what a read finds — the invariant the crate we
    /// stand in for asserts in C++ at this same point rather than reporting.
    pub fn get<'a>(&self, scope: &PinScope<'a, '_>, index: usize) -> Local<'a, Primitive> {
        let realm = crate::realm_of(scope);
        match api::Array::get(&realm, self.engine(), index as u32) {
            Ok(value) => {
                let value: Local<'_, Value> = Local::from_engine(value);
                value.retag()
            }
            Err(error) => {
                crate::throw(scope, &error);
                Local::from_engine(api::Local::undefined())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Data, Integer};
    use crate::test_support::{bind, eval_number, in_context};

    /// The length is the one asked for, an unwritten slot reads as *undefined*,
    /// and a written one reads back as what was written.
    #[test]
    fn a_primitive_array_holds_what_the_host_wrote() {
        in_context!(scope, {
            let array = PrimitiveArray::new(scope, 2);
            assert_eq!(array.length(), 2, "the length is the one asked for");

            let unwritten: Local<'_, Value> = array.get(scope, 0).into();
            assert!(
                unwritten.is_undefined(),
                "a slot nothing wrote is undefined"
            );

            let seven = Integer::new(scope, 7);
            array.set(scope, 0, seven.cast::<Primitive>());

            let read: Local<'_, Value> = array.get(scope, 0).into();
            bind(scope, "slot", read);
            assert_eq!(eval_number(scope, "slot"), 7.0);
            let still: Local<'_, Value> = array.get(scope, 1).into();
            assert!(still.is_undefined(), "writing one slot leaves the others");
        });
    }

    /// The trip the host this stands in for makes with one: a `PrimitiveArray`
    /// crosses into `Data` as `host_defined_options` and comes back out of it,
    /// which is what the unchecked cast at the far end relies on.
    #[test]
    fn a_primitive_array_survives_the_trip_as_data() {
        in_context!(scope, {
            let options = PrimitiveArray::new(scope, 1);
            let kind = Integer::new_from_unsigned(scope, 2);
            options.set(scope, 0, kind.cast::<Primitive>());

            let as_data: Local<'_, Data> = options.into();
            // The host casts back without checking, because the engine only ever
            // hands this value to the callback that asked for it — the same
            // unchecked trip `deno_core` makes with a `transmute`, which this is
            // the bridge's own form of (`Data` to `PrimitiveArray` is one of the
            // casts §9 records as deliberately absent).
            let back: Local<'_, PrimitiveArray> = as_data.retag();

            assert_eq!(back.length(), 1);
            let read: Local<'_, Value> = back.get(scope, 0).into();
            bind(scope, "kind", read);
            assert_eq!(eval_number(scope, "kind"), 2.0);
        });
    }
}
