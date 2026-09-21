//! Contexts: the realm a scope operates on (`v8::Context`).

use runtime::api;

use crate::data::{Context, Object, ObjectTemplate};
use crate::handle::{Local, Payload};
use crate::scope::PinScope;

/// Options for [`Context::new`] (`v8::ContextOptions`).
///
/// Slag has no realm configuration yet; the field exists because hosts set it.
#[derive(Default)]
pub struct ContextOptions<'s> {
    pub global_template: Option<Local<'s, ObjectTemplate>>,
}

impl Context {
    /// Create a realm on the scope's isolate and leave it entered.
    ///
    /// The engine pushes the realm's bootstrap execution context when the
    /// context is created, so with one context per isolate the context is
    /// current for as long as it exists.
    #[allow(clippy::new_ret_no_self)]
    pub fn new<'s>(
        scope: &PinScope<'s, '_, ()>,
        _options: ContextOptions<'_>,
    ) -> Local<'s, Context> {
        let mut isolate = scope.isolate_ptr();
        let context = api::Context::new(isolate.engine_mut())
            .expect("bridge: creating a realm cannot fail outside OOM");
        // The engine's isolate owns the realm, so this handle only names it;
        // recording it here is what lets a scope-less operation find it.
        isolate.set_current_context(Some(context));
        // The engine makes the new realm current on its isolate; mirror that
        // here so operations with no scope to read it from can find it.
        crate::realm::enter(context);
        Local::from_payload(Payload::Context(context))
    }
}

impl<'s> Local<'s, Context> {
    /// The context's global object (`v8::Context::Global`).
    pub fn global(&self, _scope: &PinScope<'s, '_, ()>) -> Local<'s, Object> {
        Local::from_engine(self.context().global())
    }
}
