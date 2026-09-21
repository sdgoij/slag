//! Contexts: the realm a scope operates on (`v8::Context`).

use std::ffi::c_void;

use runtime::api;

use crate::data::{Context, Object, ObjectTemplate};
use crate::handle::{Local, LocalHandle, Payload};
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

impl<'s> LocalHandle<'s, Context> {
    /// The context's global object (`v8::Context::Global`).
    pub fn global(&self, _scope: &PinScope<'s, '_, ()>) -> Local<'s, Object> {
        Local::from_engine(self.context().global())
    }

    /// Store a host pointer in a slot on this context
    /// (`v8::Context::SetAlignedPointerInEmbedderData`).
    ///
    /// Slag's contexts have no embedder-data slots, so the bridge keeps them,
    /// on the isolate and keyed by this context. A slot that was never written
    /// reads back null, which is what the crate we stand in for answers too.
    pub fn set_aligned_pointer_in_embedder_data(&self, index: i32, value: *mut c_void) {
        self.slots_isolate()
            .set_context_slot(self.identity(), index, value as usize);
    }

    /// The host pointer in a slot on this context
    /// (`v8::Context::GetAlignedPointerFromEmbedderData`), null when nothing was
    /// stored there.
    pub fn get_aligned_pointer_from_embedder_data(&self, index: i32) -> *mut c_void {
        self.slots_isolate().context_slot(self.identity(), index) as *mut c_void
    }

    /// Forget every slot written on this context
    /// (`v8::Context::ClearAllSlots`).
    ///
    /// The crate clears the *values* it keeps for the context, including its own
    /// bookkeeping; the slots here are the host's pointers, so this forgets the
    /// pointers and the host keeps what they point at — which is the same
    /// contract, one side of it written down.
    pub fn clear_all_slots(&self) {
        self.slots_isolate().clear_context_slots(self.identity());
    }

    /// The isolate the slots live on.
    fn slots_isolate(&self) -> crate::Isolate {
        // SAFETY: a context lives in the agent of a live isolate, and the engine
        // isolate is the first field of `IsolateInner`, which is what makes the
        // two addresses the same — see `Isolate::from_engine_ptr`.
        unsafe { crate::Isolate::from_engine_ptr(self.context().isolate()) }
    }

    /// What this context's slots are keyed by: its global object's id, which is
    /// how the bridge already tells two contexts apart.
    fn identity(&self) -> u64 {
        self.context()
            .global()
            .value()
            .as_object()
            .map(|object| object.id())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    use crate::test_support::in_context;

    /// A host pointer lands in the slot it was put in and only there, and a slot
    /// the host never wrote reads back null rather than a stale neighbour.
    #[test]
    fn a_host_pointer_round_trips_through_a_context_slot() {
        in_context!(scope, {
            let context = scope.get_current_context();
            let pointer = 0x1234usize as *mut c_void;

            assert!(context.get_aligned_pointer_from_embedder_data(3).is_null());

            context.set_aligned_pointer_in_embedder_data(3, pointer);
            assert_eq!(context.get_aligned_pointer_from_embedder_data(3), pointer);
            assert!(context.get_aligned_pointer_from_embedder_data(4).is_null());

            // A second slot, so one write cannot be mistaken for another.
            let other = 0x5678usize as *mut c_void;
            context.set_aligned_pointer_in_embedder_data(4, other);
            assert_eq!(context.get_aligned_pointer_from_embedder_data(3), pointer);
            assert_eq!(context.get_aligned_pointer_from_embedder_data(4), other);
        });
    }

    /// Clearing the slots forgets every index on this context, and nothing else.
    #[test]
    fn clearing_the_slots_forgets_what_was_written() {
        in_context!(scope, {
            let context = scope.get_current_context();
            let pointer = 0x1234usize as *mut c_void;
            context.set_aligned_pointer_in_embedder_data(1, pointer);
            context.set_aligned_pointer_in_embedder_data(2, pointer);
            assert_eq!(context.get_aligned_pointer_from_embedder_data(1), pointer);

            context.clear_all_slots();
            assert!(context.get_aligned_pointer_from_embedder_data(1).is_null());
            assert!(context.get_aligned_pointer_from_embedder_data(2).is_null());

            // And a write after the clear lands where it should, so the map is
            // still usable rather than merely emptied.
            context.set_aligned_pointer_in_embedder_data(2, pointer);
            assert_eq!(context.get_aligned_pointer_from_embedder_data(2), pointer);
        });
    }
}
