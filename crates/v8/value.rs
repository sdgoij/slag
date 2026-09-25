//! The `Value` surface: predicates and conversions (`v8::Value`).
//!
//! Every predicate that asks what a value *is* delegates to [`predicates`],
//! which is also what the tag casts in [`crate::data`] check against, so a
//! `TryFrom` cast and the corresponding `is_*` cannot disagree.

use crate::data::{self as predicates, BigInt, Boolean, Integer, Number, Object, String, Value};
use crate::handle::{Local, LocalHandle};
use crate::scope::PinScope;

use runtime::api;

impl<'s> LocalHandle<'s, Value> {
    pub fn is_undefined(&self) -> bool {
        self.engine().is_undefined()
    }

    pub fn is_null(&self) -> bool {
        self.engine().is_null()
    }

    pub fn is_null_or_undefined(&self) -> bool {
        self.is_null() || self.is_undefined()
    }

    pub fn is_true(&self) -> bool {
        self.engine().as_boolean() == Some(true)
    }

    pub fn is_false(&self) -> bool {
        self.engine().as_boolean() == Some(false)
    }

    /// A string or a symbol, which is what `v8::Name` names.
    pub fn is_name(&self) -> bool {
        predicates::is_name(self.engine())
    }

    pub fn is_string(&self) -> bool {
        self.engine().is_string()
    }

    pub fn is_symbol(&self) -> bool {
        self.engine().is_symbol()
    }

    pub fn is_function(&self) -> bool {
        self.engine().is_function()
    }

    pub fn is_array(&self) -> bool {
        predicates::is_array(self.engine())
    }

    pub fn is_object(&self) -> bool {
        predicates::is_object(self.engine())
    }

    pub fn is_big_int(&self) -> bool {
        self.engine().is_bigint()
    }

    pub fn is_boolean(&self) -> bool {
        self.engine().is_boolean()
    }

    pub fn is_number(&self) -> bool {
        self.engine().is_number()
    }

    pub fn is_external(&self) -> bool {
        predicates::is_external(self.engine())
    }

    pub fn is_int32(&self) -> bool {
        predicates::is_int32(self.engine())
    }

    pub fn is_uint32(&self) -> bool {
        predicates::is_uint32(self.engine())
    }

    pub fn is_date(&self) -> bool {
        predicates::is_date(self.engine())
    }

    pub fn is_arguments_object(&self) -> bool {
        predicates::is_arguments_object(self.engine())
    }

    pub fn is_big_int_object(&self) -> bool {
        predicates::is_big_int_object(self.engine())
    }

    pub fn is_boolean_object(&self) -> bool {
        predicates::is_boolean_object(self.engine())
    }

    pub fn is_number_object(&self) -> bool {
        predicates::is_number_object(self.engine())
    }

    /// A `String` object, not a string primitive.
    pub fn is_string_object(&self) -> bool {
        predicates::is_string_object(self.engine())
    }

    pub fn is_symbol_object(&self) -> bool {
        predicates::is_symbol_object(self.engine())
    }

    pub fn is_native_error(&self) -> bool {
        predicates::is_native_error(self.engine())
    }

    pub fn is_reg_exp(&self) -> bool {
        predicates::is_reg_exp(self.engine())
    }

    pub fn is_async_function(&self) -> bool {
        predicates::is_async_function(self.engine())
    }

    pub fn is_generator_function(&self) -> bool {
        predicates::is_generator_function(self.engine())
    }

    pub fn is_promise(&self) -> bool {
        predicates::is_promise(self.engine())
    }

    pub fn is_map(&self) -> bool {
        predicates::is_map(self.engine())
    }

    pub fn is_set(&self) -> bool {
        predicates::is_set(self.engine())
    }

    pub fn is_map_iterator(&self) -> bool {
        predicates::is_map_iterator(self.engine())
    }

    pub fn is_set_iterator(&self) -> bool {
        predicates::is_set_iterator(self.engine())
    }

    pub fn is_generator_object(&self) -> bool {
        predicates::is_generator_object(self.engine())
    }

    pub fn is_weak_map(&self) -> bool {
        predicates::is_weak_map(self.engine())
    }

    pub fn is_weak_set(&self) -> bool {
        predicates::is_weak_set(self.engine())
    }

    pub fn is_array_buffer(&self) -> bool {
        predicates::is_array_buffer(self.engine())
    }

    pub fn is_array_buffer_view(&self) -> bool {
        predicates::is_array_buffer_view(self.engine())
    }

    pub fn is_typed_array(&self) -> bool {
        predicates::is_typed_array(self.engine())
    }

    pub fn is_int8_array(&self) -> bool {
        predicates::is_int8_array(self.engine())
    }

    pub fn is_uint8_array(&self) -> bool {
        predicates::is_uint8_array(self.engine())
    }

    pub fn is_uint8_clamped_array(&self) -> bool {
        predicates::is_uint8_clamped_array(self.engine())
    }

    pub fn is_int16_array(&self) -> bool {
        predicates::is_int16_array(self.engine())
    }

    pub fn is_uint16_array(&self) -> bool {
        predicates::is_uint16_array(self.engine())
    }

    pub fn is_int32_array(&self) -> bool {
        predicates::is_int32_array(self.engine())
    }

    pub fn is_uint32_array(&self) -> bool {
        predicates::is_uint32_array(self.engine())
    }

    pub fn is_float16_array(&self) -> bool {
        predicates::is_float16_array(self.engine())
    }

    pub fn is_float32_array(&self) -> bool {
        predicates::is_float32_array(self.engine())
    }

    pub fn is_float64_array(&self) -> bool {
        predicates::is_float64_array(self.engine())
    }

    pub fn is_big_int64_array(&self) -> bool {
        predicates::is_big_int64_array(self.engine())
    }

    pub fn is_big_uint64_array(&self) -> bool {
        predicates::is_big_uint64_array(self.engine())
    }

    pub fn is_data_view(&self) -> bool {
        predicates::is_data_view(self.engine())
    }

    pub fn is_shared_array_buffer(&self) -> bool {
        predicates::is_shared_array_buffer(self.engine())
    }

    pub fn is_proxy(&self) -> bool {
        predicates::is_proxy(self.engine())
    }

    pub fn is_module_namespace_object(&self) -> bool {
        predicates::is_module_namespace_object(self.engine())
    }

    pub fn is_constructor(&self) -> bool {
        self.engine().is_constructor()
    }

    /// Strict equality (spec 7.2.13). Engine values compare structurally, which
    /// for `Rc`-backed handles means reference identity for objects — and `-0`
    /// equal to `0`, and `NaN` to nothing.
    pub fn strict_equals(&self, that: Local<'s, Value>) -> bool {
        self.engine() == that.engine()
    }

    /// SameValue (spec 7.2.14): strict equality with `NaN` equal to itself and
    /// `-0` distinct from `0`, which is what `Object.is` asks.
    pub fn same_value(&self, that: Local<'s, Value>) -> bool {
        crux::ops::same_value(self.engine().value(), that.engine().value())
    }

    /// SameValueZero (spec 7.2.10): as [`same_value`](Self::same_value) with `-0`
    /// equal to `0`, which is what a keyed collection asks.
    pub fn same_value_zero(&self, that: Local<'s, Value>) -> bool {
        crux::ops::same_value_zero(self.engine().value(), that.engine().value())
    }

    /// A name for the type of this value, for error messages: the chain the
    /// crate we stand in for uses, without the two wasm brands, which the
    /// engine's tables only carry when it is built with wasm.
    pub fn type_repr(&self) -> &'static str {
        let value = self.engine();
        if predicates::is_module_namespace_object(value) {
            "Module"
        } else if predicates::is_proxy(value) {
            "Proxy"
        } else if predicates::is_shared_array_buffer(value) {
            "SharedArrayBuffer"
        } else if predicates::is_data_view(value) {
            "DataView"
        } else if predicates::is_big_uint64_array(value) {
            "BigUint64Array"
        } else if predicates::is_big_int64_array(value) {
            "BigInt64Array"
        } else if predicates::is_float64_array(value) {
            "Float64Array"
        } else if predicates::is_float32_array(value) {
            "Float32Array"
        } else if predicates::is_int32_array(value) {
            "Int32Array"
        } else if predicates::is_uint32_array(value) {
            "Uint32Array"
        } else if predicates::is_int16_array(value) {
            "Int16Array"
        } else if predicates::is_uint16_array(value) {
            "Uint16Array"
        } else if predicates::is_int8_array(value) {
            "Int8Array"
        } else if predicates::is_uint8_clamped_array(value) {
            "Uint8ClampedArray"
        } else if predicates::is_uint8_array(value) {
            "Uint8Array"
        } else if predicates::is_typed_array(value) {
            "TypedArray"
        } else if predicates::is_array_buffer_view(value) {
            "ArrayBufferView"
        } else if predicates::is_array_buffer(value) {
            "ArrayBuffer"
        } else if predicates::is_weak_set(value) {
            "WeakSet"
        } else if predicates::is_weak_map(value) {
            "WeakMap"
        } else if predicates::is_set_iterator(value) {
            "Set Iterator"
        } else if predicates::is_map_iterator(value) {
            "Map Iterator"
        } else if predicates::is_set(value) {
            "Set"
        } else if predicates::is_map(value) {
            "Map"
        } else if predicates::is_promise(value) {
            "Promise"
        } else if predicates::is_generator_function(value) {
            "Generator function"
        } else if predicates::is_async_function(value) {
            "Async function"
        } else if predicates::is_reg_exp(value) {
            "RegExp"
        } else if predicates::is_date(value) {
            "Date"
        } else if predicates::is_number(value) {
            "Number"
        } else if predicates::is_boolean(value) {
            "Boolean"
        } else if predicates::is_big_int(value) {
            "bigint"
        } else if predicates::is_array(value) {
            "array"
        } else if predicates::is_function(value) {
            "function"
        } else if predicates::is_symbol(value) {
            "symbol"
        } else if predicates::is_string(value) {
            "string"
        } else if value.is_null() {
            "null"
        } else if value.is_undefined() {
            "undefined"
        } else {
            "unknown"
        }
    }

    /// ToBigInt (`v8::Value::ToBigInt`). A failing conversion leaves a pending
    /// exception, which is where a `Number` or a `Symbol` lands.
    pub fn to_big_int<'a>(&self, scope: &PinScope<'a, '_>) -> Option<Local<'a, BigInt>> {
        let value = *self.engine().value();
        let realm = crate::realm_of(scope);
        match realm.with_agent(|agent| runtime::context::to_big_int(agent, &value)) {
            Ok(big) => Some(crate::bigint::from_engine_int(big)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// ToObject (`v8::Value::ToObject`). A receiver is itself; every other value
    /// is wrapped, and `null`/`undefined` fail with a pending exception.
    pub fn to_object<'a>(&self, scope: &PinScope<'a, '_>) -> Option<Local<'a, Object>> {
        let value = *self.engine().value();
        let realm = crate::realm_of(scope);
        match realm.with_agent(|agent| runtime::context::to_object(agent, &value)) {
            Ok(object) => Some(Local::from_engine(api::Local::from(object))),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// ToBoolean (`v8::Value::ToBoolean`). Every value has one, so this cannot
    /// fail and has no error channel.
    pub fn to_boolean<'a>(&self, scope: &PinScope<'a, '_>) -> Local<'a, Boolean> {
        Boolean::new(scope, self.boolean_value(scope))
    }

    /// The value's truthiness (`v8::Value::BooleanValue`).
    pub fn boolean_value(&self, _scope: &PinScope<'_, '_>) -> bool {
        crux::convert::to_boolean(self.engine().value())
    }

    /// ToInteger (`v8::Value::ToInteger`).
    ///
    /// The value is exact where the crate we stand in for's handle would not be:
    /// its `Integer` is a 32-bit slot, so an integral value outside that range
    /// does not survive the cast it does here, while this bridge's
    /// [`Local<Integer>::value`] reads an `i64`.
    pub fn to_integer<'a>(&self, scope: &PinScope<'a, '_>) -> Option<Local<'a, Integer>> {
        let number = self.number_value(scope)?;
        Some(Local::from_engine(api::Local::number(
            crux::convert::to_integer_or_infinity(number),
        )))
    }

    /// The value as a number (`v8::Value::NumberValue`): a number is itself, and
    /// anything else is `ToNumber`'d — which is where a `Symbol` or a `BigInt`
    /// fails, with the pending exception the conversion left.
    pub fn number_value(&self, scope: &PinScope<'_, '_>) -> Option<f64> {
        if let Some(number) = self.engine().as_number() {
            return Some(number);
        }
        Some(self.to_number(scope)?.value())
    }

    /// ToNumber (`v8::Value::ToNumber`). A failing conversion leaves a pending
    /// exception.
    pub fn to_number<'a>(&self, scope: &PinScope<'a, '_>) -> Option<Local<'a, Number>> {
        let value = *self.engine().value();
        let realm = crate::realm_of(scope);
        match realm.with_agent(|agent| runtime::context::to_number(agent, &value)) {
            Ok(number) => Some(Number::new(scope, number)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// The value as an `i64` (`v8::Value::IntegerValue`): a number is truncated
    /// toward zero, and anything else is `ToInteger`'d first, which is where a
    /// failing conversion leaves its exception.
    pub fn integer_value(&self, scope: &PinScope<'_, '_>) -> Option<i64> {
        let number = self.number_value(scope)?;
        Some(crux::convert::to_integer_or_infinity(number) as i64)
    }

    /// The value as an `i32` (`v8::Value::Int32Value`).
    pub fn int32_value(&self, scope: &PinScope<'_, '_>) -> Option<i32> {
        let number = self.number_value(scope)?;
        Some(crux::convert::to_int32(number))
    }

    /// The value as a `u32` (`v8::Value::Uint32Value`).
    pub fn uint32_value(&self, scope: &PinScope<'_, '_>) -> Option<u32> {
        let number = self.number_value(scope)?;
        Some(crux::convert::to_uint32(number))
    }

    /// ToString (spec 7.1.17). A failing conversion leaves a pending exception.
    pub fn to_string<'a>(&self, scope: &PinScope<'a, '_>) -> Option<Local<'a, String>> {
        let value = *self.engine();
        let realm = crate::realm_of(scope);
        match realm.with_agent(|agent| runtime::context::to_string(agent, value.value())) {
            Ok(text) => Some(Local::from_engine(api::Local::from(
                crux::value::Value::String(crux::handle::Handle::new(text)),
            ))),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// `v8::Value::ToRustStringLossy`: the string rendering, or an empty string
    /// when the conversion throws.
    pub fn to_rust_string_lossy(&self, scope: &PinScope<'_, '_>) -> std::string::String {
        self.to_string(scope)
            .map_or_else(std::string::String::new, |text| {
                text.to_rust_string_lossy(scope)
            })
    }

    /// `typeof` as a string (`v8::Value::TypeOf`, spec 7.2.6).
    ///
    /// The engine's own answer rather than a spelling assembled here: it is the
    /// one place that knows an `IsHTMLDDA` object is `undefined` and a callable
    /// proxy is `function`, which is the same table the `typeof` operator reads.
    pub fn type_of<'a>(&self, _scope: &PinScope<'a, '_>) -> Local<'a, String> {
        Local::from_engine(api::Local::string(self.engine().type_of()))
    }

    /// `this instanceof constructor` (`v8::Value::InstanceOf`, spec 7.3.20).
    ///
    /// The whole operator, so a constructor whose `@@hasInstance` is a method
    /// answers through it exactly as it does in a script. `None` is a thrown
    /// `TypeError` — a non-object or non-callable right-hand side, or a trap that
    /// threw — with the exception pending.
    pub fn instance_of<'a>(
        &self,
        scope: &PinScope<'a, '_>,
        object: Local<'a, Object>,
    ) -> Option<bool> {
        let value = *self.engine().value();
        let constructor = *object.engine().value();
        let realm = crate::realm_of(scope);
        match realm.with_agent(|agent| runtime::expr::instance_of(agent, &constructor, &value)) {
            Ok(result) => Some(crux::convert::to_boolean(&result)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Object;
    use crate::test_support::{eval, in_context};

    /// `type_of` is the operator's own answer (spec 7.2.6), including the two
    /// the engine's table decides rather than the value's tag.
    #[test]
    fn type_of_is_the_operators_answer() {
        in_context!(scope, {
            for (source, expected) in [
                ("undefined", "undefined"),
                ("null", "object"),
                ("true", "boolean"),
                ("1", "number"),
                ("1n", "bigint"),
                ("'x'", "string"),
                ("Symbol('s')", "symbol"),
                ("(() => {})", "function"),
                ("[]", "object"),
                // A callable proxy is a function, not an object.
                ("new Proxy(() => {}, {})", "function"),
            ] {
                let answer = eval(scope, source)
                    .type_of(scope)
                    .to_rust_string_lossy(scope);
                assert_eq!(answer, expected, "typeof {source}");
            }
        });
    }

    /// `instance_of` is the whole `InstanceofOperator` (spec 7.3.20): the
    /// prototype walk, an `@@hasInstance` override, and a thrown `TypeError` for
    /// a right-hand side that can do neither.
    #[test]
    fn instance_of_is_the_whole_operator() {
        in_context!(scope, {
            let array_ctor = Local::<Object>::try_from(eval(scope, "Array")).expect("Array");

            let array = Local::<Object>::try_from(eval(scope, "[]")).expect("array");
            assert_eq!(array.instance_of(scope, array_ctor), Some(true));

            let plain = Local::<Object>::try_from(eval(scope, "({})")).expect("object");
            assert_eq!(plain.instance_of(scope, array_ctor), Some(false));

            // The override: a plain object whose `@@hasInstance` says yes to an
            // argument no prototype walk would accept.
            let liar =
                Local::<Object>::try_from(eval(scope, "({ [Symbol.hasInstance]: () => true })"))
                    .expect("object");
            assert_eq!(eval(scope, "1").instance_of(scope, liar), Some(true));

            // Neither callable nor carrying `@@hasInstance`: a `TypeError`, which
            // is this API's empty answer with the exception pending.
            let not_callable = Local::<Object>::try_from(eval(scope, "({})")).expect("object");
            assert_eq!(eval(scope, "1").instance_of(scope, not_callable), None);
        });
    }
}
