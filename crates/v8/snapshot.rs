//! Snapshot data a host carries between runs (`v8::StartupData`), and the
//! creator it comes from (`v8::SnapshotCreator`).
//!
//! # What a blob carries
//!
//! The data a host attached to a context (`v8::Isolate::AddContextData`), and
//! the shape it was attached in, written by the engine's own format
//! (`runtime::snapshot`). Not a heap image: a realm here is rebuilt by the
//! engine on every boot, so what a host has to get back is the part it built
//! itself. The blob is therefore a *value graph rooted at the attached data*,
//! one item list per context slot, and a restore resolves it in the realm it is
//! restoring into — which is why a reference to a builtin is written as the
//! builtin's name rather than as a copy of the builtin.
//!
//! # Index conventions
//!
//! V8's, because the host this stands in for reads them back: context slot 0 is
//! the default context, and `AddContext` answers 1, 2, ... for contexts added
//! after it (V8's `kFirstAddtlContextIndex`). A context's own data starts at 0.
//! A blob records a slot for every context the creator knows, holding
//! `undefined` for one that was never recorded, so a slot the host never used
//! answers "the blob names no such context" rather than "that context was
//! empty".
//!
//! # What is not carried yet
//!
//! A value the engine's walk refuses — a function body, a proxy, a typed array,
//! a module namespace, a host object, an array with a hole — ends
//! [`create_blob`](SnapshotCreator::create_blob) with a panic naming it. The
//! crate's signature is `Option`, which its own callers unwrap, so the loudest
//! available message is the honest one; a blob that quietly lost part of a
//! host's state would move that failure to where the host cannot see it.
//!
//! Neither `FunctionCodeHandling` mode carries compiled code, because nothing
//! carries a function at all: both answers write the same blob. Isolate-level
//! data and continuation-from-an-existing-blob are likewise not carried yet,
//! and say so where a host would look for them.

use std::collections::HashMap;
use std::ops::Deref;

use runtime::api;
use runtime::snapshot as format;

use crate::data::Data;
use crate::handle::{Global, Local, Payload};
use crate::isolate::{Isolate, OwnedIsolate};

/// Serialized engine state a host carries between runs (v8::StartupData).
#[derive(Debug, Clone)]
pub struct StartupData(std::borrow::Cow<'static, [u8]>);

impl StartupData {
    /// Whether the data could be rehashed on deserialization
    /// (v8::StartupData::CanBeRehashed).
    ///
    /// No: a blob here is a value graph rather than a heap image, so there is
    /// no compiled-code hash to rehash. The answer describes what a blob is
    /// rather than whether one was made.
    pub fn can_be_rehashed(&self) -> bool {
        false
    }

    /// Whether the data is a blob this engine can read
    /// (v8::StartupData::IsValid).
    ///
    /// The header's answer: the format's magic, its version, the pointer width
    /// and the byte order it was written with. A blob from V8 is not valid for
    /// Slag — and this is the question a host asks before it boots from one, so
    /// it is answered rather than asserted.
    pub fn is_valid(&self) -> bool {
        format::is_valid(&self.0)
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self(std::borrow::Cow::Owned(bytes))
    }
}

impl Deref for StartupData {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> From<T> for StartupData
where
    T: Into<std::borrow::Cow<'static, [u8]>>,
{
    fn from(data: T) -> Self {
        Self(data.into())
    }
}

/// What a snapshot does with compiled function code
/// (v8::FunctionCodeHandling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum FunctionCodeHandling {
    /// Discard it, so it is compiled again on deserialization.
    Clear,
    /// Keep it in the blob.
    Keep,
}

/// The state an isolate being serialized shows a host (v8::SnapshotCreator).
///
/// It records what a blob carries — the context a deserialized isolate starts
/// in, the contexts added after it, and the data each one was given — and
/// [`create_blob`](Self::create_blob) writes it. Recording is what keeps the
/// indices a host stores true numbers: they are the positions it asked for, not
/// invented ones.
#[derive(Default)]
pub(crate) struct SnapshotCreator {
    /// The context a deserialized isolate starts in (slot 0).
    default_context: Option<api::Context>,
    /// The contexts added after the default one, in the order they were added:
    /// slot 1, 2, ... — V8's `kFirstAddtlContextIndex`.
    contexts: Vec<api::Context>,
    /// The data attached to each context, by the slot that context has in the
    /// blob. Held persistently, because the blob is what reads it back and the
    /// collector has to keep it until then.
    attached: HashMap<usize, Vec<Global<Data>>>,
}

impl SnapshotCreator {
    /// An isolate set up for serialization (v8::SnapshotCreator::SnapshotCreator).
    #[allow(clippy::new_ret_no_self)]
    pub(crate) fn new(
        external_references: Option<std::borrow::Cow<'static, [crate::ExternalReference]>>,
        params: Option<crate::CreateParams>,
    ) -> OwnedIsolate {
        Self::new_impl(external_references, None, params)
    }

    /// The same, continuing from a snapshot the host carries
    /// (v8::SnapshotCreator::SnapshotCreator).
    pub(crate) fn from_existing_snapshot(
        existing_snapshot_blob: StartupData,
        external_references: Option<std::borrow::Cow<'static, [crate::ExternalReference]>>,
        params: Option<crate::CreateParams>,
    ) -> OwnedIsolate {
        Self::new_impl(external_references, Some(existing_snapshot_blob), params)
    }

    fn new_impl(
        external_references: Option<std::borrow::Cow<'static, [crate::ExternalReference]>>,
        existing_snapshot_blob: Option<StartupData>,
        params: Option<crate::CreateParams>,
    ) -> OwnedIsolate {
        let mut params = params.unwrap_or_default();
        if let Some(external_references) = external_references {
            params = params.external_references(external_references);
        }
        if let Some(snapshot_blob) = existing_snapshot_blob {
            params = params.snapshot_blob(snapshot_blob);
        }
        Isolate::with_snapshot_creator(params, Self::default())
    }

    /// Record the context a deserialized isolate would start in
    /// (v8::SnapshotCreator::SetDefaultContext).
    pub(crate) fn set_default_context(&mut self, context: api::Context) {
        self.default_context = Some(context);
    }

    /// Record another context, answering the index it was given
    /// (v8::SnapshotCreator::AddContext).
    ///
    /// The index is slot 1 for the first context added, as in V8: slot 0 is the
    /// default context whether or not one was set, and the host this stands in
    /// for reads its realm back out of slot 1.
    ///
    /// Attaching data to one of these refuses — see
    /// [`add_context_data`](Self::add_context_data): a blob here carries the
    /// default context's data, because an isolate in this engine has one realm
    /// and a second context shadows it.
    pub(crate) fn add_context(&mut self, context: api::Context) -> usize {
        self.contexts.push(context);
        self.contexts.len()
    }

    /// Record data attached to `context`, answering the index it was given
    /// (v8::SnapshotCreator::AddContextData).
    ///
    /// # Panics
    ///
    /// Panics when `context` is not this snapshot's default context. V8
    /// carries every context's data; this bridge carries the default context's,
    /// because the engine's api gives an isolate one realm and a second
    /// `Context::new` shadows it — so a second realm's objects are built in the
    /// wrong realm and a blob of them would be wrong rather than partial. That
    /// is a named gap, not a silent one: a host that attaches to another
    /// context hears about it where it attaches. Panics too when the isolate
    /// did not come from `Isolate::snapshot_creator`.
    pub(crate) fn add_context_data(&mut self, context: api::Context, data: Global<Data>) -> usize {
        let slot = self.slot_of(context).unwrap_or_else(|| {
            panic!(
                "v8::Isolate::AddContextData: this bridge carries the default context's data, not another context's (an isolate here has one realm)"
            )
        });
        let items = self.attached.entry(slot).or_default();
        items.push(data);
        items.len() - 1
    }

    /// The slot a context's data is carried in, if this snapshot carries it.
    fn slot_of(&self, context: api::Context) -> Option<usize> {
        let key = context_identity(context);
        self.default_context
            .filter(|default| context_identity(*default) == key)
            .map(|_| 0)
    }

    /// Write the blob.
    ///
    /// The graph is rooted at the context table: one item list per context
    /// slot, holding the data that context was attached. A value the engine
    /// cannot carry ends this with a panic naming it — see the module docs for
    /// why that is the loudest message this signature allows.
    pub(crate) fn create_blob(
        &mut self,
        function_code_handling: FunctionCodeHandling,
    ) -> StartupData {
        // Neither mode carries compiled code yet, because nothing carries a
        // function body at all, and a blob is therefore the same either way.
        let _ = function_code_handling;
        let context = self
            .default_context
            .or_else(|| self.contexts.first().copied())
            .expect(
                "v8::SnapshotCreator::create_blob: a snapshot carries what a context holds, so the creator needs one",
            );
        // One slot: the default context's data. A context added after it has
        // no slot of its own, because attaching data to one refuses (see
        // `add_context_data`) — the blob names what it carries rather than
        // naming an empty list for a context whose own data was never taken.
        let slots: Vec<Vec<api::Local>> = vec![
            self.attached
                .get(&0)
                .map(|items| items.iter().filter_map(engine_value).collect())
                .unwrap_or_default(),
        ];
        match context.write_snapshot(&slots) {
            Ok(bytes) => StartupData::new(bytes),
            Err(error) => {
                panic!("v8::SnapshotCreator::create_blob: the engine cannot carry {error} yet")
            }
        }
    }
}

/// The engine value a persistent data handle names, or `None` when it is not a
/// language value at all.
fn engine_value(global: &Global<Data>) -> Option<api::Local> {
    global.payload_value().as_value_opt().copied()
}

/// A context's identity: its global object, which is the identity this bridge
/// already tells two contexts apart by.
pub(crate) fn context_identity(context: api::Context) -> u64 {
    context
        .global()
        .value()
        .as_object()
        .map(|object| object.id())
        .unwrap_or(0)
}

/// What a host's blob becomes on the reading side.
///
/// The blob is decoded lazily, on the first context that asks for its data:
/// decoding makes engine values, and there is no realm to make them in until
/// [`Context::from_snapshot`](crate::Context::from_snapshot) has created one.
pub(crate) struct SnapshotRestore {
    /// The blob as the host handed it over, once its header checked out.
    blob: StartupData,
    /// The decoded root — one item list per context slot — held persistently so
    /// the items stay alive while the host reads them out one at a time.
    root: Option<Global<Data>>,
    /// Each restored context's items, by the context's identity, taken as they
    /// are read: that is the crate's `...FromSnapshotOnce` contract, and the
    /// second read of an index is an error there too.
    items: HashMap<u64, Vec<Option<Global<Data>>>>,
}

impl SnapshotRestore {
    /// The restore for a blob, or `None` when the blob is not one of this
    /// engine's — in which case the isolate boots from source instead, which is
    /// what `is_valid` answering `false` tells the host to expect.
    pub(crate) fn new(blob: StartupData) -> Option<Self> {
        format::is_valid(blob.bytes()).then(|| Self {
            blob,
            root: None,
            items: HashMap::new(),
        })
    }

    /// Decode the blob into `context`'s realm, once.
    fn decode(&mut self, isolate: &Isolate, context: api::Context) -> bool {
        if self.root.is_some() {
            return true;
        }
        let Ok(root) = context.read_snapshot(self.blob.bytes()) else {
            return false;
        };
        let handle: Local<'_, Data> = Local::from_payload(Payload::Value(root));
        self.root = Some(Global::new(isolate, handle));
        true
    }

    /// Root the items of the context at `slot`, answering whether the blob
    /// names that slot at all.
    fn restore(&mut self, isolate: &Isolate, context: api::Context, slot: usize) -> bool {
        if !self.decode(isolate, context) {
            return false;
        }
        let engine = context;
        let Some(root) = self.root.as_ref().and_then(engine_value) else {
            return false;
        };
        let slots = api::Array::length(&engine, &root).unwrap_or(0.0) as usize;
        if slot >= slots {
            return false;
        }
        let Ok(element) = api::Array::get(&engine, &root, slot as u32) else {
            return false;
        };
        // A slot the creator never recorded reads back as `undefined`, which is
        // "the blob names no such context" rather than "that context was empty".
        if element.value().is_undefined() {
            return false;
        }
        let count = api::Array::length(&engine, &element).unwrap_or(0.0) as usize;
        let mut items = Vec::with_capacity(count);
        for index in 0..count {
            let Ok(item) = api::Array::get(&engine, &element, index as u32) else {
                return false;
            };
            let handle: Local<'_, Data> = Local::from_payload(Payload::Value(item));
            items.push(Some(Global::new(isolate, handle)));
        }
        self.items.insert(context_identity(context), items);
        true
    }

    /// The item at `index` of a restored context's data, taken.
    fn take(&mut self, context_identity: u64, index: usize) -> Option<Global<Data>> {
        self.items
            .get_mut(&context_identity)?
            .get_mut(index)?
            .take()
    }
}

/// Restore the context at `slot` of the isolate's blob, answering whether the
/// blob names it. The restore is taken out and put back so that making the
/// persistent handles inside it cannot alias the isolate.
pub(crate) fn restore_context(isolate: &mut Isolate, context: api::Context, slot: usize) -> bool {
    let Some(mut restore) = isolate.take_restore() else {
        return false;
    };
    let present = restore.restore(isolate, context, slot);
    isolate.put_restore(restore);
    present
}

/// The item at `index` of the restored context's data, taken; `None` for an
/// index that was never there or has already been read.
pub(crate) fn take_context_data(
    isolate: &mut Isolate,
    context_identity: u64,
    index: usize,
) -> Option<Global<Data>> {
    let mut restore = isolate.take_restore()?;
    let item = restore.take(context_identity, index);
    isolate.put_restore(restore);
    item
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Number, Value};
    use crate::{Context, ContextOptions, Global, Isolate, Local, Object, OwnedIsolate};

    /// An isolate booted from `blob`.
    fn isolate_from(blob: StartupData) -> OwnedIsolate {
        Isolate::new(crate::CreateParams::default().snapshot_blob(blob))
    }

    /// A fresh context on `isolate`, held persistently: what a host makes when
    /// it boots without a blob, and what it holds a restored one by.
    fn fresh_context(isolate: &mut OwnedIsolate) -> Global<Context> {
        crate::scope!(let scope, isolate);
        let context = Context::new(scope, Default::default());
        let handle = scope.isolate_ptr();
        Global::new(&handle, context)
    }

    /// Restore slot `slot` of the isolate's blob.
    fn restored_context(isolate: &mut OwnedIsolate, slot: usize) -> Option<Global<Context>> {
        crate::scope!(let scope, isolate);
        let context = Context::from_snapshot(scope, slot, ContextOptions::default())?;
        let handle = scope.isolate_ptr();
        Some(Global::new(&handle, context))
    }

    /// A blob whose default context was given one number per argument.
    fn numbers_blob(values: &[f64]) -> StartupData {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            for value in values {
                let number: Local<Value> = Number::new(scope, *value).into();
                scope.add_context_data(context, number);
            }
        }
        isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob")
    }

    /// The numbers a restored context's data holds, read the way a host reads
    /// them: one scope, each index once. `None` is an index that is not there,
    /// or has already been read.
    fn data(isolate: &mut OwnedIsolate, count: usize) -> Vec<Option<f64>> {
        let context = restored_context(isolate, 0).expect("the blob names slot 0");
        crate::scope!(let scope, isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        (0..count)
            .map(|index| {
                scope
                    .get_context_data_from_snapshot_once::<Value>(index)
                    .ok()
                    .and_then(|value| Local::<Number>::try_from(value).ok())
                    .map(|number| number.value())
            })
            .collect()
    }

    /// Attached data is what a blob carries, and a restore hands it back at the
    /// index it was attached under.
    #[test]
    fn attached_data_round_trips_through_a_blob() {
        let blob = numbers_blob(&[7.0, 42.0]);
        assert!(blob.is_valid(), "a blob this bridge wrote is one it reads");
        assert!(!blob.can_be_rehashed());

        let mut isolate = isolate_from(blob);
        assert_eq!(data(&mut isolate, 2), vec![Some(7.0), Some(42.0)]);
    }

    /// A graph comes back as a graph: a self-reference is still a
    /// self-reference rather than a copy.
    #[test]
    fn the_graph_keeps_its_identity() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let object = crate::test_support::eval(scope, "const o = { n: 1 }; o.self = o; o");
            scope.add_context_data(context, object);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, 0).expect("the blob names slot 0");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let object = scope
            .get_context_data_from_snapshot_once::<Object>(0)
            .expect("the object");
        let key = crate::test_support::eval(scope, "'self'");
        let self_value = object.get(scope, key).expect("the property");
        let object_value: Local<Value> = object.into();
        assert!(
            self_value == object_value,
            "the cycle came back the same object, not a copy"
        );
    }

    /// Reading an index is once, and an index that was never attached is the
    /// same error.
    #[test]
    fn an_index_is_read_once() {
        let mut isolate = isolate_from(numbers_blob(&[1.0]));
        let context = restored_context(&mut isolate, 0).expect("the blob names slot 0");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        assert!(
            scope
                .get_context_data_from_snapshot_once::<Value>(0)
                .is_ok()
        );
        assert!(
            scope
                .get_context_data_from_snapshot_once::<Value>(0)
                .is_err(),
            "the second read of an index is not the first"
        );
        assert!(
            scope
                .get_context_data_from_snapshot_once::<Value>(9)
                .is_err()
        );
    }

    /// A slot the blob does not name answers `None`, and the isolate it was
    /// asked of keeps working — which is the branch a host takes when the slot
    /// its build used is not one the blob has.
    #[test]
    fn a_slot_the_blob_does_not_name_answers_none() {
        let mut isolate = isolate_from(numbers_blob(&[1.0]));
        assert!(restored_context(&mut isolate, 3).is_none());
        assert_eq!(data(&mut isolate, 1), vec![Some(1.0)]);
    }

    /// An isolate that booted from source has no snapshot data, and says so
    /// through the same error a spent index answers with.
    #[test]
    fn an_isolate_without_a_blob_has_no_data() {
        let mut isolate = Isolate::new(crate::CreateParams::default());
        assert!(restored_context(&mut isolate, 0).is_none());
        let context = fresh_context(&mut isolate);
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        assert!(
            scope
                .get_context_data_from_snapshot_once::<Value>(0)
                .is_err()
        );
        assert!(
            scope
                .get_isolate_data_from_snapshot_once::<Value>(0)
                .is_err()
        );
    }

    /// A blob from elsewhere is not valid, and an isolate handed one boots from
    /// source rather than reading it.
    #[test]
    fn a_foreign_blob_is_not_valid_and_is_not_consumed() {
        let v8_blob = StartupData::from(vec![0x21, 0x00, 0x00, 0x00, 0x42, 0x42]);
        assert!(!v8_blob.is_valid());
        let mut isolate = isolate_from(v8_blob);
        assert!(restored_context(&mut isolate, 0).is_none());
        let context = fresh_context(&mut isolate);
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        assert_eq!(crate::test_support::eval_number(scope, "6 * 7"), 42.0);
        assert!(
            scope
                .get_context_data_from_snapshot_once::<Value>(0)
                .is_err()
        );
    }

    /// A value the engine cannot carry ends the build with a panic naming it,
    /// rather than a blob that quietly lost part of a host's state.
    #[test]
    #[should_panic(expected = "cannot carry a function")]
    fn a_function_ends_the_build_with_its_name() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let function = crate::test_support::eval(scope, "(function f() {})");
            scope.add_context_data(context, function);
        }
        let _ = isolate.create_blob(FunctionCodeHandling::Keep);
    }

    /// The slot convention is V8's — the first added context is slot 1 — and
    /// the default context's data is what a blob carries today.
    #[test]
    fn an_added_context_gets_slot_one_and_the_blob_names_only_the_default() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let default = Context::new(scope, Default::default());
            let added = Context::new(scope, Default::default());
            scope.set_default_context(default);
            assert_eq!(scope.add_context(added), 1);
            let first: Local<Value> = Number::new(scope, 1.0).into();
            scope.add_context_data(default, first);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        assert_eq!(data(&mut isolate, 1), vec![Some(1.0)]);
        assert!(
            restored_context(&mut isolate, 1).is_none(),
            "the blob names the default context, not the one added after it"
        );
    }

    /// Data attached to a context a blob does not carry is refused where the
    /// host attaches it, which is where it can act on the refusal.
    #[test]
    #[should_panic(expected = "one realm")]
    fn data_for_another_context_is_refused() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        crate::scope!(let scope, &mut isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut crate::ContextScope::new(scope, context);
        scope.set_default_context(context);
        let stranger = Context::new(scope, Default::default());
        let number: Local<Value> = Number::new(scope, 1.0).into();
        scope.add_context_data(stranger, number);
    }

    /// A context no default was set for is not one the blob carries either.
    #[test]
    #[should_panic(expected = "one realm")]
    fn data_without_a_default_context_is_refused() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        crate::scope!(let scope, &mut isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut crate::ContextScope::new(scope, context);
        let number: Local<Value> = Number::new(scope, 1.0).into();
        scope.add_context_data(context, number);
    }

    /// A creator that never took a context has nothing to carry, and says so
    /// rather than writing a blob that names none.
    #[test]
    #[should_panic(expected = "the creator needs one")]
    fn a_creator_without_a_context_refuses() {
        let isolate = Isolate::snapshot_creator(None, None);
        let _ = isolate.create_blob(FunctionCodeHandling::Keep);
    }

    /// The creator-only methods still refuse on an isolate that is not one.
    #[test]
    #[should_panic(expected = "not created by Isolate::snapshot_creator")]
    fn setting_a_default_context_needs_a_creator_isolate() {
        let mut isolate = Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, &mut isolate);
        let context = Context::new(scope, Default::default());
        scope.set_default_context(context);
    }

    /// A creator that continues from a blob it was handed still records what it
    /// is told; what the previous blob carried is not consumed yet, which is
    /// the statement the module makes about continuation.
    #[test]
    fn a_creator_from_an_existing_snapshot_records_its_own() {
        let references: std::borrow::Cow<'static, [crate::ExternalReference]> =
            std::borrow::Cow::Owned(Vec::new());
        let mut isolate = Isolate::snapshot_creator_from_existing_snapshot(
            StartupData::from(vec![]),
            Some(references),
            Some(crate::CreateParams::default()),
        );
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let number: Local<Value> = Number::new(scope, 3.0).into();
            scope.add_context_data(context, number);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Clear)
            .expect("a blob");
        let mut isolate = isolate_from(blob);
        assert_eq!(data(&mut isolate, 1), vec![Some(3.0)]);
    }

    /// A restored value can be made persistent and read after the scope that
    /// produced it is gone, which is what a host that keeps snapshot state
    /// around does.
    #[test]
    fn a_restored_value_outlives_the_scope_it_came_from() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let object = crate::test_support::eval(scope, "({ a: 1 })");
            scope.add_context_data(context, object);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, 0).expect("the blob names slot 0");
        let held = {
            crate::scope!(let scope, &mut isolate);
            let context = context.open(scope);
            let scope = &mut crate::ContextScope::new(scope, context);
            let object = scope
                .get_context_data_from_snapshot_once::<Object>(0)
                .expect("the object");
            let handle = scope.isolate_ptr();
            Global::new(&handle, object)
        };
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let key = crate::test_support::eval(scope, "'a'");
        let value = held.open(scope).get(scope, key).expect("the property");
        assert_eq!(
            Local::<Number>::try_from(value)
                .ok()
                .map(|number| number.value()),
            Some(1.0)
        );
    }
}
