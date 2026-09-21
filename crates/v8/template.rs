//! Function templates (`v8::FunctionTemplate`).
//!
//! A template is a host-side description of a function, with no engine object
//! of its own. The handle names it the way the crate we stand in for names an
//! `External`: an engine object holding the template's address, which the
//! isolate's arena owns for as long as the isolate lives.

use std::ffi::c_void;
use std::rc::Rc;

use runtime::api;

use crate::data::{External, Function, FunctionTemplate, String, Value};
use crate::function::{FunctionBuilder, FunctionCallback, FunctionCallbackInfo, SideEffectType};
use crate::handle::{Global, Local};
use crate::scope::PinScope;
use crate::support::MapFnTo;

impl FunctionTemplate {
    /// Create a template from a host function (v8::FunctionTemplate::New).
    pub fn new<'s>(
        scope: &PinScope<'s, '_, ()>,
        callback: impl MapFnTo<FunctionCallback>,
    ) -> Local<'s, FunctionTemplate> {
        Self::builder(callback).build(scope)
    }

    /// Create a template from a raw callback (v8::FunctionTemplate::New).
    pub fn new_raw<'s>(
        scope: &PinScope<'s, '_, ()>,
        callback: FunctionCallback,
    ) -> Local<'s, FunctionTemplate> {
        Self::builder_raw(callback).build(scope)
    }

    /// A builder over a mapped callback (v8::FunctionTemplate::builder, which is
    /// the same as `FunctionBuilder::<FunctionTemplate>::new`).
    pub fn builder<'s>(callback: impl MapFnTo<FunctionCallback>) -> FunctionBuilder<'s, Self> {
        FunctionBuilder::new(callback)
    }

    /// The same over a callback a host already has in the raw shape
    /// (v8::FunctionTemplate::builder_raw).
    pub fn builder_raw<'s>(callback: FunctionCallback) -> FunctionBuilder<'s, Self> {
        FunctionBuilder::new_raw(callback)
    }

    /// A template over everything the shapes that make one can set.
    ///
    /// One place a template is made, so [`FunctionTemplate::new`] and the
    /// builder cannot drift apart. Two of the crate we stand in for's
    /// parameters are absent rather than ignored: a **signature** (the engine has
    /// no access check for a host function to consult) and **fast-call
    /// overloads** (there is no compiler to generate one for, so the engine calls
    /// the host callback it was given).
    pub(crate) fn from_parts<'s>(
        scope: &PinScope<'s, '_, ()>,
        callback: FunctionCallback,
        data: Option<Local<'s, Value>>,
        length: i32,
        constructible: bool,
        side_effect_type: SideEffectType,
    ) -> Local<'s, FunctionTemplate> {
        // Accepted and not used: it says what an optimizer may assume about the
        // call, and this engine has none.
        let _ = side_effect_type;
        let mut isolate = scope.isolate_ptr();
        // The data is pinned rather than captured as a handle: the engine keeps
        // this closure for as long as the template lives, which is longer than
        // the scope the host's handle came from. Without a root, a collection
        // could free the value and the callback would be handed a value that is
        // no longer the one the function was built with.
        let data = data.map(|value| Global::new(&isolate, value));
        let engine = api::FunctionTemplate::new(
            isolate.engine_mut(),
            Box::new(move |info: &api::FunctionCallbackInfo<'_>| {
                let data = match &data {
                    Some(pinned) => pinned.handle(),
                    None => Local::from_engine(api::Local::undefined()),
                };
                let view = FunctionCallbackInfo::new(info, data);
                // SAFETY: the host handed over a plain function pointer, and
                // this view is live for the duration of the call.
                unsafe { callback(&view) };
            }),
        );
        engine.set_length(length);
        engine.set_constructible(constructible);
        let pointer = Rc::as_ptr(&engine) as *mut c_void;
        isolate.add_template(engine);
        let handle: Local<'s, External> = External::new(scope, pointer);
        // The pointer the template was just stored under, retagged: the bridge
        // put it there itself, so there is nothing for a checked cast to ask.
        handle.retag()
    }
}

impl<'s> FunctionBuilder<'s, FunctionTemplate> {
    /// Create the template (`v8::FunctionBuilder<FunctionTemplate>::build`).
    pub fn build(self, scope: &PinScope<'s, '_, ()>) -> Local<'s, FunctionTemplate> {
        self.into_template(scope)
    }
}

impl<'s> Local<'s, FunctionTemplate> {
    /// The template's `name`, and the name a `new` instance is printed under
    /// (v8::FunctionTemplate::SetClassName).
    pub fn set_class_name(&self, name: Local<'_, String>) {
        if let Some(text) = name.engine().as_string() {
            self.template().set_class_name(&text);
        }
    }

    /// The function this template makes in the scope's realm
    /// (v8::FunctionTemplate::GetFunction).
    pub fn get_function(&self, scope: &PinScope<'_, '_, ()>) -> Option<Local<'s, Function>> {
        let realm = crate::realm_of(scope);
        let value = self.template_rc().get_function(&realm).ok()?;
        let function: Local<'_, Function> = Local::from_engine(value);
        Some(function)
    }

    fn template(&self) -> &api::FunctionTemplate {
        let pointer = api::External::from(*self.engine().value()).value();
        // SAFETY: the handle was built from a template address the isolate's
        // arena owns, and the isolate outlives every handle on it.
        unsafe { &*(pointer as *const api::FunctionTemplate) }
    }

    fn template_rc(&self) -> Rc<api::FunctionTemplate> {
        let pointer = api::External::from(*self.engine().value()).value();
        // SAFETY: the pointer came from `Rc::as_ptr` on a template the arena
        // keeps alive, so the allocation is live and its count can be raised;
        // `from_raw` then takes ownership of exactly the count raised here.
        unsafe {
            Rc::increment_strong_count(pointer as *const api::FunctionTemplate);
            Rc::from_raw(pointer as *const api::FunctionTemplate)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;
    use crate::function::{FunctionCallbackArguments, ReturnValue};
    use crate::test_support::{bind, eval_number, in_context};

    /// A host function in the shape the crate we stand in for maps: a plain
    /// Rust function taking the scope, the arguments and the result slot.
    fn sum(scope: &mut PinScope<'_, '_>, args: FunctionCallbackArguments, rv: ReturnValue) {
        let total: f64 = (0..args.length())
            .filter_map(|index| {
                Local::<crate::data::Number>::try_from(args.get(index))
                    .ok()
                    .map(|number| number.value())
            })
            .sum();
        let total = crate::data::Number::new(scope, total);
        rv.set(total.into());
    }

    /// The whole callback path: a Rust function mapped to the raw callback
    /// shape, turned into a template, instantiated, and called by a script —
    /// reading `this`, the arguments and the return slot on the way.
    #[test]
    fn a_mapped_host_function_is_callable_from_a_script() {
        in_context!(scope, {
            let template = FunctionTemplate::new(scope, sum);
            let function: Local<'_, Function> = template.get_function(scope).expect("function");
            bind(scope, "sum", function.cast::<Value>());

            assert_eq!(eval_number(scope, "sum(1, 2, 3)"), 6.0);
            assert_eq!(eval_number(scope, "sum()"), 0.0);
            assert_eq!(eval_number(scope, "sum.call(null, 1.5, 2.5)"), 4.0);
        });
    }
}
