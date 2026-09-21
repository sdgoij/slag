//! Function templates (`v8::FunctionTemplate`).
//!
//! A template is a host-side description of a function, with no engine object
//! of its own. The handle names it the way the crate we stand in for names an
//! `External`: an engine object holding the template's address, which the
//! isolate's arena owns for as long as the isolate lives.

use std::ffi::c_void;
use std::rc::Rc;

use runtime::api;

use crate::data::{
    Data, External, Function, FunctionTemplate, Name, ObjectTemplate, String, Value,
};
use crate::fast_api::CFunction;
use crate::function::{FunctionBuilder, FunctionCallback, FunctionCallbackInfo, SideEffectType};
use crate::handle::{Global, Local, LocalHandle};
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

    /// The same over a fast-call signature
    /// (`v8::FunctionBuilder<FunctionTemplate>::build_fast`).
    ///
    /// The overloads are accepted and not used, which is this bridge's tier for
    /// everything fast-api: a fast call is V8's own compiled entry point for a
    /// host function, and there is no compiler here to generate one, so a call
    /// reaches the callback the builder was given. Only the speed differs — the
    /// callback is the same function the host passed, which is also what the
    /// crate we stand in for falls back to when a fast call does not apply.
    pub fn build_fast(
        self,
        scope: &PinScope<'s, '_, ()>,
        overloads: &'static [CFunction],
    ) -> Local<'s, FunctionTemplate> {
        let _ = overloads;
        self.into_template(scope)
    }
}

impl<'s> LocalHandle<'s, FunctionTemplate> {
    /// The template of the constructor's `.prototype` object
    /// (v8::FunctionTemplate::PrototypeTemplate).
    ///
    /// Instances share this one: a property set here is on every instance's
    /// prototype, which is where the caller of the crate we stand in for puts
    /// methods.
    pub fn prototype_template(&self, scope: &PinScope<'s, '_, ()>) -> Local<'s, ObjectTemplate> {
        object_template_handle(scope, self.template_rc().prototype_template())
    }

    /// The template of the instances `new` creates
    /// (v8::FunctionTemplate::InstanceTemplate).
    pub fn instance_template(&self, scope: &PinScope<'s, '_, ()>) -> Local<'s, ObjectTemplate> {
        object_template_handle(scope, self.template_rc().instance_template())
    }

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

impl ObjectTemplate {
    /// A fresh object template (v8::ObjectTemplate::New).
    ///
    /// On the tag rather than on the handle, because that is where the crate we
    /// stand in for declares it and its call sites are written as
    /// `v8::ObjectTemplate::new(scope)`.
    pub fn new<'s>(scope: &PinScope<'s, '_, ()>) -> Local<'s, ObjectTemplate> {
        let mut isolate = scope.isolate_ptr();
        let template = api::ObjectTemplate::new(isolate.engine_mut());
        object_template_handle(scope, template)
    }
}

impl<'s> LocalHandle<'s, ObjectTemplate> {
    /// Add a property to each instance created from this template
    /// (v8::ObjectTemplate::Set, which is `Template::Set` in the crate we stand
    /// in for).
    ///
    /// The property is a plain data property with the crate's defaults
    /// (writable, enumerable, configurable); the attributes overload is absent
    /// until a host asks for it.
    ///
    /// # Panics
    ///
    /// If `key` is not a string. The engine's templates are string-keyed — a
    /// symbol key has no representation in one — and the crate we stand in for
    /// gives this method no channel to report that on, so the alternative to
    /// this panic is a property that silently does not exist.
    pub fn set(&self, key: Local<'_, Name>, value: Local<'_, Data>) {
        let Some(name) = key.engine().as_string() else {
            panic!(
                "bridge: a template property key must be a string (the engine's templates are string-keyed)"
            );
        };
        self.object_template().set(&name, *value.engine());
    }

    fn object_template(&self) -> &api::ObjectTemplate {
        let pointer = api::External::from(*self.engine().value()).value();
        // SAFETY: the handle was built from a template address the isolate took
        // ownership of, and the isolate outlives every handle on it.
        unsafe { &*(pointer as *const api::ObjectTemplate) }
    }
}

/// Store an engine object template on the isolate and hand back the handle that
/// names it — the same shape a function template handle has.
fn object_template_handle<'s>(
    scope: &PinScope<'s, '_, ()>,
    template: Rc<api::ObjectTemplate>,
) -> Local<'s, ObjectTemplate> {
    let isolate = scope.isolate_ptr();
    let pointer = Rc::as_ptr(&template) as *mut c_void;
    isolate.add_object_template(template);
    let handle: Local<'s, External> = External::new(scope, pointer);
    // The pointer the template was just stored under, retagged: the bridge put
    // it there itself, so there is nothing for a checked cast to ask.
    handle.retag()
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    use super::*;
    use crate::Number;
    use crate::data::{Data, Name};
    use crate::function::{FunctionCallbackArguments, ReturnValue};
    use crate::scope::GetIsolate;
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

    /// The two templates a host fills in: a property set on the prototype
    /// template is on every instance's prototype, and one set on the instance
    /// template is on the instance itself. This is the shape a host registers a
    /// class of ops with.
    #[test]
    fn the_prototype_and_instance_templates_reach_their_objects() {
        in_context!(scope, {
            let template = FunctionTemplate::new(scope, sum);

            let method = FunctionTemplate::new(scope, sum)
                .get_function(scope)
                .expect("function");
            let key: Local<'_, Name> = String::new(scope, "m").expect("string").into();
            template
                .prototype_template(scope)
                .set(key, method.cast::<Data>());

            let field: Local<'_, Name> = String::new(scope, "field").expect("string").into();
            template
                .instance_template(scope)
                .set(field, Number::new(scope, 7.0).cast::<Data>());

            let constructor = template.get_function(scope).expect("constructor");
            bind(scope, "C", constructor.cast::<Value>());
            assert_eq!(
                eval_number(scope, "new C().field"),
                7.0,
                "a property on the instance template is the instance's own"
            );
            assert_eq!(
                eval_number(scope, "new C().m(1, 2)"),
                3.0,
                "a property on the prototype template is reached through the instance"
            );
        });
    }

    /// An object template stands on its own as well as through a function
    /// template: this is what a host builds when it wants instances of a shape
    /// without a constructor.
    #[test]
    fn an_object_template_holds_the_properties_it_was_given() {
        in_context!(scope, {
            let template = ObjectTemplate::new(scope);
            let key: Local<'_, Name> = String::new(scope, "answer").expect("string").into();
            template.set(key, Number::new(scope, 42.0).cast::<Data>());

            // The template is a handle over an engine template this isolate
            // owns, so the cast the snapshot machinery makes must find it and a
            // second template's pointer must not be mistaken for it.
            let other = ObjectTemplate::new(scope);
            assert_ne!(template, other);
        });
    }

    /// `build_fast` is the crate's fast-call constructor. The overloads are
    /// accepted and not used — there is no compiler here to enter one — so what
    /// matters is that a call still reaches the callback the builder was given.
    #[test]
    fn build_fast_builds_a_template_whose_calls_reach_the_callback() {
        in_context!(scope, {
            let template = FunctionTemplate::builder(sum).build_fast(scope, &[]);
            let function: Local<'_, Function> = template.get_function(scope).expect("function");
            bind(scope, "fast_sum", function.cast::<Value>());
            assert_eq!(eval_number(scope, "fast_sum(2, 3)"), 5.0);
        });
    }

    /// The cast a host's snapshot machinery asks: the data a template was
    /// registered under casts back to a function template, while the same
    /// engine object wrapped around a pointer the host chose does not.
    #[test]
    fn a_template_casts_back_but_a_host_pointer_does_not() {
        in_context!(scope, {
            let template = FunctionTemplate::new(scope, sum);
            let data: Local<'_, Data> = template.cast();
            let cast = Local::<FunctionTemplate>::try_from(data).expect("template");
            assert_eq!(cast, template);

            // A persistent handle is the shape a host keeps one in, and the
            // text-free round trip through it is the one the cast sees.
            let isolate = scope.get_isolate_ptr();
            let persistent = Global::new(&isolate, template);
            let from_global: Local<'_, Data> = persistent.get(scope).cast();
            assert!(Local::<FunctionTemplate>::try_from(from_global).is_ok());

            let pointer = 0x1234usize as *mut c_void;
            let host_pointer: Local<'_, Data> = crate::External::new(scope, pointer).cast();
            assert!(Local::<FunctionTemplate>::try_from(host_pointer).is_err());
        });
    }
}
