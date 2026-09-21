//! Callbacks: the shapes a host function is called through
//! (v8::FunctionCallback, v8::FunctionCallbackInfo, v8::ReturnValue).

use std::marker::PhantomData;

use runtime::api;

use crate::data::{Function, FunctionTemplate, Object, Value};
use crate::handle::Local;
use crate::isolate::{Isolate, UnsafeRawIsolatePtr};
use crate::scope::PinScope;
use crate::support::{MapFnFrom, MapFnTo, UnitType};

/// Whether a function can be called with `new` (v8::ConstructorBehavior).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstructorBehavior {
    /// Calling it with `new` throws, as it does for an arrow function.
    Throw,
    /// Calling it with `new` constructs an object.
    Allow,
}

/// What the engine may assume about a call's effects (v8::SideEffectType).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideEffectType {
    /// The call may do anything the language can observe.
    HasSideEffect,
    /// The call cannot be observed, so it may be optimized away.
    HasNoSideEffect,
    /// Only the receiver is observable, as for a property getter.
    HasSideEffectToReceiver,
}

/// The raw callback shape (v8::FunctionCallback).
///
/// The engine calls this with a pointer to the [`FunctionCallbackInfo`] of the
/// call. In the crate we stand in for, that pointer is V8's own argument
/// struct; here it is this bridge's view of the same call, which is what a
/// callback mapped from a Rust function reads.
pub type FunctionCallback = unsafe extern "C" fn(*const FunctionCallbackInfo);

/// The call a callback reads (v8::FunctionCallbackInfo).
///
/// A raw pointer and the data the function was built with. It carries no
/// lifetime because a callback's signature cannot name one: the pointer is good
/// only for the duration of the call that produced it.
#[repr(C)]
#[derive(Debug)]
pub struct FunctionCallbackInfo {
    info: *const api::FunctionCallbackInfo<'static>,
    data: Local<'static, Value>,
}

impl FunctionCallbackInfo {
    /// The bridge's view of a call the engine is making.
    pub(crate) fn new(info: &api::FunctionCallbackInfo<'_>, data: Local<'_, Value>) -> Self {
        Self {
            // The pointer stays a pointer, and going through `*const ()` is
            // what lets the engine's lifetime be erased here: the type has to
            // be nameable in a callback's signature, which cannot name it.
            info: (info as *const _ as *const ()).cast(),
            data: Local::from_payload(*data.payload()),
        }
    }

    /// The values a callback usually reads first, in one go
    /// (v8::FunctionCallbackInfo::GetParts).
    pub fn get_parts(&self) -> FunctionCallbackInfoParts<'_> {
        FunctionCallbackInfoParts {
            // SAFETY: the value is only read while the call is in progress.
            isolate: unsafe { self.isolate().as_raw_isolate_ptr() },
            return_value: self.return_value(),
            data: self.data,
            length: self.length(),
        }
    }

    pub(crate) fn isolate(&self) -> Isolate {
        // SAFETY: the engine's call is in progress, and the isolate it names
        // outlives it.
        unsafe { Isolate::from_engine_ptr(self.engine().isolate()) }
    }

    pub(crate) fn return_value(&self) -> ReturnValue<'_> {
        ReturnValue {
            info: self.info,
            marker: PhantomData,
        }
    }

    pub(crate) fn length(&self) -> i32 {
        self.engine().length() as i32
    }

    pub(crate) fn this(&self) -> Local<'_, Object> {
        let value: Local<'_, Object> = Local::from_engine(self.engine().this());
        value
    }

    pub(crate) fn get(&self, index: i32) -> Local<'_, Value> {
        let value: Local<'_, Value> = match self.engine().arg(index.max(0) as usize) {
            Some(value) => Local::from_engine(value),
            None => Local::from_engine(api::Local::undefined()),
        };
        value
    }

    pub(crate) fn is_construct_call(&self) -> bool {
        self.engine().is_construct_call()
    }

    /// The data the function was built with, or `undefined`.
    pub(crate) fn data(&self) -> Local<'_, Value> {
        self.data
    }

    fn engine(&self) -> &api::FunctionCallbackInfo<'static> {
        // SAFETY: `new` stored a reference that is live for the call, and a
        // callback only reads it while that call runs.
        unsafe { &*self.info }
    }
}

/// The values a callback reads first (v8::FunctionCallbackInfoParts).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct FunctionCallbackInfoParts<'cb> {
    pub isolate: UnsafeRawIsolatePtr,
    pub return_value: ReturnValue<'cb>,
    pub data: Local<'cb, Value>,
    pub length: i32,
}

/// The arguments of a call (v8::FunctionCallbackArguments).
///
/// What is pre-read is optional: a callback that already has the
/// [`FunctionCallbackInfoParts`] hands them over, so the argument count and the
/// callback data are not read twice.
#[derive(Debug)]
pub struct FunctionCallbackArguments<'s> {
    info: &'s FunctionCallbackInfo,
    data: Option<Local<'s, Value>>,
    length: Option<i32>,
}

impl<'s> FunctionCallbackArguments<'s> {
    /// The arguments for a call, reading everything from the call itself.
    pub fn from_function_callback_info(info: &'s FunctionCallbackInfo) -> Self {
        Self {
            info,
            data: None,
            length: None,
        }
    }

    /// The arguments for a call whose parts are already in hand, which is how
    /// generated ops start.
    pub fn from_function_callback_info_parts(
        info: &'s FunctionCallbackInfo,
        parts: &FunctionCallbackInfoParts<'s>,
    ) -> Self {
        Self {
            info,
            data: Some(parts.data),
            length: Some(parts.length),
        }
    }

    /// The receiver: the value before the dot, or `this`.
    pub fn this(&self) -> Local<'s, Object> {
        self.info.this()
    }

    /// The data the function was built with, or `undefined`.
    pub fn data(&self) -> Local<'s, Value> {
        self.data.unwrap_or_else(|| self.info.data())
    }

    /// The number of arguments passed.
    pub fn length(&self) -> i32 {
        self.length.unwrap_or_else(|| self.info.length())
    }

    /// The argument at `i`, or `undefined` when there is none.
    pub fn get(&self, i: i32) -> Local<'s, Value> {
        self.info.get(i)
    }

    /// Whether the call is a construct (`new`).
    pub fn is_construct_call(&self) -> bool {
        self.info.is_construct_call()
    }
}

/// Where a callback puts its result (v8::ReturnValue).
///
/// Unset leaves `undefined`, as it does there.
pub struct ReturnValue<'s, T = Value> {
    info: *const api::FunctionCallbackInfo<'static>,
    marker: PhantomData<(&'s (), T)>,
}

impl<T> Clone for ReturnValue<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ReturnValue<'_, T> {}

impl<T> std::fmt::Debug for ReturnValue<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReturnValue")
    }
}

impl<'s, T> ReturnValue<'s, T> {
    /// Set the call's result.
    pub fn set(&self, value: Local<'s, T>) {
        // SAFETY: the callback is running, so the engine's call view is live.
        unsafe { &*self.info }
            .get_return_value()
            .set(value.into_engine());
    }

    /// Set the call's result to `undefined`.
    pub fn set_undefined(&self) {
        // SAFETY: as `set`.
        unsafe { &*self.info }.get_return_value().set_undefined();
    }

    /// Set the call's result to `null`.
    pub fn set_null(&self) {
        // SAFETY: as `set`.
        unsafe { &*self.info }.get_return_value().set_null();
    }
}

impl<F> MapFnFrom<F> for FunctionCallback
where
    F: UnitType
        + for<'s, 'i> Fn(&mut PinScope<'s, 'i>, FunctionCallbackArguments<'s>, ReturnValue<'s>),
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const FunctionCallbackInfo)
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    FunctionCallbackArguments<'s>,
                    ReturnValue<'s>,
                ),
        {
            // SAFETY: the engine hands a callback a pointer to a call view it
            // keeps alive for the call, and this adapter only reads it here.
            let info = unsafe { &*info };
            crate::callback_scope!(unsafe scope, info);
            let args = FunctionCallbackArguments::from_function_callback_info(info);
            let value = info.return_value();
            (F::get())(scope, args, value);
        }

        adapter::<F>
    }
}

/// A builder for a host function's properties (`v8::FunctionBuilder`).
///
/// Every setting has a default, and the defaults are what the plain
/// [`FunctionTemplate::new`](crate::FunctionTemplate::new) shape produces, so a
/// host that wants none of them has no reason to build one.
pub struct FunctionBuilder<'s, T> {
    callback: FunctionCallback,
    data: Option<Local<'s, Value>>,
    length: i32,
    constructor_behavior: ConstructorBehavior,
    side_effect_type: SideEffectType,
    /// `fn() -> T` rather than `T`: the tag selects which `build` is in scope,
    /// and nothing here holds a `T`.
    marker: PhantomData<fn() -> T>,
}

impl<'s, T> FunctionBuilder<'s, T> {
    /// A builder over a callback mapped from a host function
    /// (`v8::FunctionBuilder::new`).
    pub fn new(callback: impl MapFnTo<FunctionCallback>) -> Self {
        Self::new_raw(callback.map_fn_to())
    }

    /// The same, over a callback a host already has in the raw shape
    /// (`v8::FunctionBuilder::new_raw`).
    pub fn new_raw(callback: FunctionCallback) -> Self {
        Self {
            callback,
            data: None,
            length: 0,
            constructor_behavior: ConstructorBehavior::Allow,
            side_effect_type: SideEffectType::HasSideEffect,
            marker: PhantomData,
        }
    }

    /// The value the function carries, which its callback reads through
    /// [`FunctionCallbackArguments::data`] (`v8::FunctionBuilder::data`).
    pub fn data(mut self, data: Local<'s, Value>) -> Self {
        self.data = Some(data);
        self
    }

    /// The function's `length`, as an own property of the function
    /// (`v8::FunctionBuilder::length`).
    ///
    /// A negative length is taken as zero; the default is zero.
    pub fn length(mut self, length: i32) -> Self {
        self.length = length;
        self
    }

    /// Whether the function may be called with `new`
    /// (`v8::FunctionBuilder::constructor_behavior`).
    pub fn constructor_behavior(mut self, constructor_behavior: ConstructorBehavior) -> Self {
        self.constructor_behavior = constructor_behavior;
        self
    }

    /// What the engine may assume about the call's effects
    /// (`v8::FunctionBuilder::side_effect_type`).
    ///
    /// Carried, and it changes nothing, which is worth being precise about: the
    /// setting tells V8's optimizer what it may assume about a call, and this
    /// engine has no optimizer, so no observable behaviour depends on it. The
    /// other settings here are properties of the function itself and are
    /// implemented for that reason.
    pub fn side_effect_type(mut self, side_effect_type: SideEffectType) -> Self {
        self.side_effect_type = side_effect_type;
        self
    }

    /// The template this builder describes. One place both `build`s go through,
    /// so a setting cannot be honoured by one and dropped by the other.
    pub(crate) fn into_template(self, scope: &PinScope<'s, '_, ()>) -> Local<'s, FunctionTemplate> {
        crate::FunctionTemplate::from_parts(
            scope,
            self.callback,
            self.data,
            self.length,
            matches!(self.constructor_behavior, ConstructorBehavior::Allow),
            self.side_effect_type,
        )
    }
}

impl<'s> FunctionBuilder<'s, Function> {
    /// Create the function in the scope's realm
    /// (`v8::FunctionBuilder<Function>::build`), `None` when the realm cannot
    /// produce one (a pending exception says why).
    ///
    /// Going through a template is what the crate we stand in for does — its
    /// `Function::New` is a `FunctionTemplate` plus `GetFunction`.
    pub fn build(self, scope: &PinScope<'s, '_, ()>) -> Option<Local<'s, Function>> {
        self.into_template(scope).get_function(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Number;
    use crate::test_support::{bind, eval_number, in_context};

    /// A value set on a fresh object, for a function to carry as its data.
    ///
    /// Built through the engine's object model rather than `Object::new` + `set`,
    /// so the helper does not have to name the scope flavour its callers hold.
    fn marked<'s, C>(_scope: &PinScope<'s, '_, C>, mark: f64) -> Local<'s, Object> {
        let object = crux::object::JsObject::ordinary_object_create(None);
        let key = crux::property::PropertyKey::from_utf8("mark");
        let defined = object.define_property_key(
            &key,
            &crux::property::PropertyDescriptor::data(crux::value::Value::Number(mark)),
        );
        assert!(
            matches!(defined, Ok(true)),
            "defining the test object's mark"
        );
        Local::from_engine(runtime::api::Local::from(crux::value::Value::Object(
            object,
        )))
    }

    /// The value the function was built with, as a `mark` property so identity is
    /// checked rather than trusted.
    fn probe(scope: &mut PinScope<'_, '_>, args: FunctionCallbackArguments, rv: ReturnValue) {
        let data = Local::<Object>::try_from(args.data()).expect("the value the builder was given");
        let key = crate::data::String::new(scope, "mark").expect("string");
        rv.set(data.get(scope, key.into()).expect("mark"));
    }

    /// A callback that answers whether it was handed data at all.
    fn plain_probe(scope: &mut PinScope<'_, '_>, args: FunctionCallbackArguments, rv: ReturnValue) {
        let undefined = args.data().is_undefined();
        let value = Number::new(scope, if undefined { 1.0 } else { 0.0 });
        rv.set(value.into());
    }

    /// A callback that does nothing, for the settings a test reads off the
    /// function rather than off a call.
    fn noop(_scope: &mut PinScope<'_, '_>, _args: FunctionCallbackArguments, _rv: ReturnValue) {}

    unsafe extern "C" fn noop_raw(_info: *const FunctionCallbackInfo) {}

    /// The value the builder was given reaches the callback as its data — where
    /// before this the callback was handed `undefined` whatever the builder set.
    #[test]
    fn a_built_function_hands_its_callback_the_data_it_was_built_with() {
        in_context!(scope, {
            let data = marked(scope, 7.0);
            let function = FunctionBuilder::<Function>::new(probe)
                .data(data.cast::<Value>())
                .build(scope)
                .expect("function");
            bind(scope, "probe", function.cast::<Value>());
            assert_eq!(eval_number(scope, "probe()"), 7.0);

            // A function built without data hands the callback `undefined`,
            // which is what the crate we stand in for answers there.
            let plain = FunctionBuilder::<Function>::new(plain_probe)
                .build(scope)
                .expect("function");
            bind(scope, "plain", plain.cast::<Value>());
            assert_eq!(eval_number(scope, "plain()"), 1.0);
        });
    }

    /// The data is a *root*, not a handle the scope happens to keep alive: a
    /// precise collection with no roots of its own leaves the value the callback
    /// reads the one the function was built with.
    #[test]
    fn the_data_a_built_function_carries_survives_a_collection() {
        in_context!(scope, {
            let data = marked(scope, 11.0);
            let function = FunctionBuilder::<Function>::new(probe)
                .data(data.cast::<Value>())
                .build(scope)
                .expect("function");
            // Bound, so the collection below cannot sweep the function itself —
            // what it tests is the data, which nothing but the pin roots.
            bind(scope, "probe", function.cast::<Value>());
            assert_eq!(eval_number(scope, "probe()"), 11.0);

            // A precise major collection with no roots of its own and no stack
            // scan: the pin the builder took is the only thing keeping the data
            // alive.
            crux::heap::with_heap_mut(|heap| {
                heap.collect(&[]);
            });
            // The churn is what gives this test teeth. A swept box keeps its
            // bytes until something reuses the slot, so a read straight after the
            // collection proves nothing — it has to come after the slot could
            // have been handed out again. (Same detector as the L1 notes: weaker
            // than the collector's own swept list, which `crux` can read and this
            // crate cannot.)
            for index in 1..=200 {
                let _churn = marked(scope, index as f64);
            }

            assert_eq!(eval_number(scope, "probe()"), 11.0);
        });
    }

    /// The length is an own property of the function, as it is there, so a host
    /// that sets one can read it back from JavaScript.
    #[test]
    fn a_built_function_reports_the_length_it_was_built_with() {
        in_context!(scope, {
            let function = FunctionBuilder::<Function>::new(noop)
                .length(3)
                .build(scope)
                .expect("function");
            bind(scope, "f", function.cast::<Value>());

            assert_eq!(eval_number(scope, "f.length"), 3.0);
            // As a builtin's length is: not writable, not enumerable, configurable.
            assert_eq!(
                eval_number(
                    scope,
                    "Object.getOwnPropertyDescriptor(f, 'length').writable ? 1 : 0"
                ),
                0.0
            );
        });
    }

    /// A function built with `ConstructorBehavior::Throw` has no [[Construct]],
    /// so `new` on it throws; the default builds one that can be constructed.
    #[test]
    fn a_non_constructible_function_refuses_new() {
        in_context!(scope, {
            let throwing = FunctionBuilder::<Function>::new(noop)
                .constructor_behavior(ConstructorBehavior::Throw)
                .build(scope)
                .expect("function");
            let allowing = FunctionBuilder::<Function>::new(noop)
                .build(scope)
                .expect("function");
            bind(scope, "throwing", throwing.cast::<Value>());
            bind(scope, "allowing", allowing.cast::<Value>());

            let does_new_throw = |name: &str| {
                eval_number(
                    scope,
                    &format!(
                        "(function () {{ try {{ new {name}(); return 0 }} catch (e) {{ return 1 }} }})()"
                    ),
                )
            };
            assert_eq!(does_new_throw("throwing"), 1.0);
            assert_eq!(does_new_throw("allowing"), 0.0);
        });
    }

    /// The template path honours the same settings, because both `build`s go
    /// through one place.
    #[test]
    fn a_built_template_carries_the_settings_to_its_function() {
        in_context!(scope, {
            let data = marked(scope, 5.0);
            let template = FunctionTemplate::builder_raw(noop_raw)
                .data(data.cast::<Value>())
                .length(2)
                .constructor_behavior(ConstructorBehavior::Throw)
                .side_effect_type(SideEffectType::HasNoSideEffect)
                .build(scope);
            let function = template.get_function(scope).expect("function");
            bind(scope, "t", function.cast::<Value>());

            assert_eq!(eval_number(scope, "t.length"), 2.0);
            assert_eq!(
                eval_number(
                    scope,
                    "(function () { try { new t(); return 0 } catch (e) { return 1 } })()"
                ),
                1.0
            );
        });
    }
}
