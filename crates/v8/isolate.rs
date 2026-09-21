//! The isolate: Slag's agent, plus the bridge's record of the realm currently
//! entered.

use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

use runtime::api;

use crate::data::Value;
use crate::handle::Local;

/// Create parameters (`v8::CreateParams`). Slag has nothing to configure yet,
/// so this exists so that `Isolate::new(CreateParams::default())` compiles.
#[derive(Debug, Default)]
pub struct CreateParams;

/// A heap and execution state (`v8::Isolate`).
///
/// The engine's isolate is heap-allocated and never moved — contexts hold raw
/// pointers to it — which is why [`OwnedIsolate`] is a `Box`.
#[repr(C)]
pub struct Isolate {
    engine: api::Isolate,
    /// The realm most recently entered through a `ContextScope`. The engine
    /// tracks its own current realm; this is the bridge's copy of it, so a
    /// scope can hand object operations the context they need.
    context: RefCell<Option<Rc<api::Context>>>,
}

impl Isolate {
    /// A fresh isolate. A context must be created before anything can run,
    /// which is the same rule the crate we stand in for has.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(_params: CreateParams) -> OwnedIsolate {
        let engine = *api::Isolate::new();
        OwnedIsolate(Box::new(Self {
            engine,
            context: RefCell::new(None),
        }))
    }

    /// `v8::Isolate::SetData`.
    pub fn set_data(&self, slot: u32, data: usize) {
        self.engine.set_data(slot, data);
    }

    /// `v8::Isolate::GetData`.
    pub fn get_data(&self, slot: u32) -> Option<usize> {
        self.engine.get_data(slot)
    }

    /// `v8::Isolate::ThrowException`.
    pub fn throw_exception(&self, exception: Local<'_, Value>) {
        self.engine
            .throw_exception(exception.into_engine().into_value());
    }

    /// Drain the microtask and job queues (`v8::Isolate::PerformMicrotaskCheckpoint`).
    pub fn run_microtasks(&mut self) -> Result<(), crux::error::JsError> {
        self.engine.run_microtasks()
    }

    pub(crate) fn engine(&self) -> &api::Isolate {
        &self.engine
    }

    pub(crate) fn engine_mut(&mut self) -> &mut api::Isolate {
        &mut self.engine
    }

    pub(crate) fn current_context(&self) -> Option<Rc<api::Context>> {
        self.context.borrow().clone()
    }

    pub(crate) fn set_current_context(&self, context: Option<Rc<api::Context>>) {
        *self.context.borrow_mut() = context;
    }
}

/// An owned isolate (`v8::OwnedIsolate`), as returned by [`Isolate::new`].
pub struct OwnedIsolate(Box<Isolate>);

impl Deref for OwnedIsolate {
    type Target = Isolate;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for OwnedIsolate {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
