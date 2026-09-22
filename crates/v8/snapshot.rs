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
//! A JavaScript function is carried as the source text it can be re-parsed from,
//! so a restored function is callable and a host's attached callbacks come back.
//! What the source cannot say is the scope the function closed over: a restored
//! function resolves a free name through the realm's global environment, and a
//! host callback — a Rust closure in this bridge — has no source at all.
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
//! A value the engine's walk refuses — a proxy, a typed array, a module
//! namespace, a host object, an array with a hole, a host callback, a bound
//! function — ends [`create_blob`](SnapshotCreator::create_blob) with a panic
//! naming it. The crate's signature is `Option`, which its own callers unwrap,
//! so the loudest available message is the honest one; a blob that quietly lost
//! part of a host's state would move that failure to where the host cannot see
//! it.
//!
//! Neither `FunctionCodeHandling` mode carries compiled code, because a blob
//! holds the source a function is rebuilt from rather than a compiled body:
//! both answers write the same blob. Isolate-level data and
//! continuation-from-an-existing-blob are likewise not carried yet, and say so
//! where a host would look for them.

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
    /// The host's external-reference table, which `create_blob` writes a host
    /// pointer as an index into. The host supplies it here and rebuilds it for
    /// every load, which is the whole point of an index: an address is a
    /// property of the process, not of the data.
    externals: Vec<crate::ExternalReference>,
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
        let mut externals = Vec::new();
        if let Some(external_references) = external_references {
            externals = external_references.clone().into_owned();
            params = params.external_references(external_references);
        }
        if let Some(snapshot_blob) = existing_snapshot_blob {
            params = params.snapshot_blob(snapshot_blob);
        }
        Isolate::with_snapshot_creator(
            params,
            SnapshotCreator {
                externals,
                ..SnapshotCreator::default()
            },
        )
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
    pub(crate) fn add_context(&mut self, context: api::Context) -> usize {
        self.contexts.push(context);
        self.contexts.len()
    }

    /// Record data attached to `context`, answering the index it was given
    /// (v8::SnapshotCreator::AddContextData).
    ///
    /// # Panics
    ///
    /// Panics when `context` is not one this snapshot carries — the statement
    /// V8's own check makes: data attached to a context a blob does not name
    /// would have nowhere to be read back out of. Panics too when the isolate
    /// did not come from `Isolate::snapshot_creator`.
    pub(crate) fn add_context_data(&mut self, context: api::Context, data: Global<Data>) -> usize {
        let slot = self.slot_of(context).unwrap_or_else(|| {
            panic!(
                "v8::Isolate::AddContextData: the context is not one this snapshot carries: call set_default_context or add_context for it first"
            )
        });
        let items = self.attached.entry(slot).or_default();
        items.push(data);
        items.len() - 1
    }

    /// The slot a context's data is carried in, if this snapshot carries it:
    /// slot 0 for the default context, then the ones added after it.
    fn slot_of(&self, context: api::Context) -> Option<usize> {
        let key = context_identity(context);
        if self
            .default_context
            .is_some_and(|default| context_identity(default) == key)
        {
            return Some(0);
        }
        self.contexts
            .iter()
            .position(|added| context_identity(*added) == key)
            .map(|position| position + 1)
    }

    /// Write the blob.
    ///
    /// The graph is the context table: the default context's attached data at
    /// slot 0, then each context added after it at the index `add_context`
    /// answered. A value the engine cannot carry ends this with a panic naming
    /// it — see the module docs for why that is the loudest message this
    /// signature allows.
    pub(crate) fn create_blob(
        &mut self,
        function_code_handling: FunctionCodeHandling,
    ) -> StartupData {
        // Neither mode carries compiled code: a function is carried as the
        // source it is rebuilt from, so a blob is the same either way.
        let _ = function_code_handling;
        assert!(
            self.default_context.is_some() || !self.contexts.is_empty(),
            "v8::SnapshotCreator::create_blob: a snapshot carries what a context holds, so the creator needs one"
        );
        let mut slots: Vec<(usize, api::Context, Vec<api::Local>)> = Vec::new();
        if let Some(default) = self.default_context {
            slots.push((0, default, self.items_of(0)));
        }
        for position in 0..self.contexts.len() {
            let slot = position + 1;
            slots.push((slot, self.contexts[position], self.items_of(slot)));
        }
        match api::Context::write_snapshot(&slots, &addresses(&self.externals)) {
            Ok(bytes) => StartupData::new(bytes),
            Err(error) => {
                panic!("v8::SnapshotCreator::create_blob: the engine cannot carry {error} yet")
            }
        }
    }

    /// The engine values attached to one slot, in the order they were attached.
    fn items_of(&self, slot: usize) -> Vec<api::Local> {
        self.attached
            .get(&slot)
            .map(|items| items.iter().filter_map(engine_value).collect())
            .unwrap_or_default()
    }
}

/// The engine value a persistent data handle names, or `None` when it is not a
/// language value at all.
fn engine_value(global: &Global<Data>) -> Option<api::Local> {
    global.payload_value().as_value_opt().copied()
}

/// The addresses a table holds, in the form the engine's snapshot operations
/// take them.
///
/// Every field of the union is one pointer — the type owns a test that says so —
/// so reading the pointer field is reading the entry the host filled in,
/// whatever kind of pointer it was.
fn addresses(references: &[crate::ExternalReference]) -> Vec<*mut std::ffi::c_void> {
    references
        .iter()
        .map(|entry| unsafe { entry.pointer })
        .collect()
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
    /// The blob as the host handed it over, once its header checked out. Kept
    /// rather than decoded: decoding makes values in a realm, and a realm exists
    /// only once a context asks for its data.
    blob: StartupData,
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
            items: HashMap::new(),
        })
    }

    /// Root the items of the context at `slot`, answering whether the blob
    /// names that slot at all.
    ///
    /// The decode happens here rather than at isolate creation because it makes
    /// engine values, and the realm to make them in is the one the restored
    /// context was created with — which does not exist until the host asks for
    /// the context.
    /// # Panics
    ///
    /// Panics when the blob's header checked out but its body cannot be read
    /// for this context: the host's external-reference table does not match the
    /// one the blob was written against, or the blob is not the one its header
    /// claims. That is a host contract violation rather than a value a host can
    /// act on, and the crate we stand in for's own loader checks in the same
    /// situation — where answering `None` would send the host down its "boot
    /// from source" branch with no way to learn why.
    fn restore(&mut self, isolate: &Isolate, context: api::Context, slot: usize) -> bool {
        let table = addresses(isolate.externals());
        let items = match context.read_snapshot(self.blob.bytes(), slot, &table) {
            Ok(Some(items)) => items,
            Ok(None) => return false,
            Err(error) => panic!(
                "v8::Context::FromSnapshot: the snapshot could not be read for context {slot}: {error}"
            ),
        };
        let mut held = Vec::with_capacity(items.len());
        for item in items {
            let handle: Local<'_, Data> = Local::from_payload(Payload::Value(item.get()));
            held.push(Some(Global::new(isolate, handle)));
        }
        self.items.insert(context_identity(context), held);
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

    /// A JavaScript function is carried as the source text it can be re-parsed
    /// from and comes back callable from a script, which is the shape
    /// `deno_core` builds: it attaches functions to the realm it snapshotted and
    /// calls them after the restore.
    #[test]
    fn a_function_round_trips_and_is_callable_from_a_script() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let function =
                crate::test_support::eval(scope, "(function addOne(a) { return a + 1; })");
            scope.add_context_data(context, function);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, 0).expect("the blob names slot 0");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let function = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the function");
        crate::test_support::bind(scope, "restored", function);
        assert_eq!(
            crate::test_support::eval_number(scope, "restored(41)"),
            42.0
        );
    }

    /// A bound function comes back callable too, which is the value `deno_core`
    /// reaches through the module map it attaches to its bootstrapped realm —
    /// the kind that stopped its build before this.
    #[test]
    fn a_bound_function_round_trips_and_is_callable_from_a_script() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let bound = crate::test_support::eval(
                scope,
                "(function add(a, b) { return a + b; }).bind(null, 1)",
            );
            scope.add_context_data(context, bound);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, 0).expect("the blob names slot 0");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let bound = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the bound function");
        crate::test_support::bind(scope, "restored", bound);
        assert_eq!(crate::test_support::eval_number(scope, "restored(2)"), 3.0);
    }

    /// A realm's global function property is carried by name rather than as a
    /// callback, and is callable after the restore — the value deno's attached
    /// graph reaches through `globalThis`.
    #[test]
    fn a_global_function_round_trips_and_is_callable_from_a_script() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let function = crate::test_support::eval(scope, "isFinite");
            scope.add_context_data(context, function);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, 0).expect("the blob names slot 0");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let function = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the global function");
        crate::test_support::bind(scope, "restored", function);
        assert_eq!(
            crate::test_support::eval_number(scope, "restored(1) ? 1 : 0"),
            1.0
        );
        assert_eq!(
            crate::test_support::eval_number(scope, "restored(Infinity) ? 1 : 0"),
            0.0
        );
    }

    /// A host callback is a Rust closure rather than source text, so it still
    /// ends the build, by name: this is the function kind the external-reference
    /// table's `function` field is for, and it is not wired to real ops yet.
    #[test]
    #[should_panic(expected = "cannot carry a built-in function")]
    fn a_host_callback_ends_the_build_with_its_name() {
        fn an_op(
            _scope: &mut crate::scope::PinScope<'_, '_>,
            _args: crate::function::FunctionCallbackArguments,
            _rv: crate::function::ReturnValue,
        ) {
        }
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let template = crate::FunctionTemplate::new(scope, an_op);
            let function = template.get_function(scope).expect("function");
            scope.add_context_data(context, function.cast::<Value>());
        }
        let _ = isolate.create_blob(FunctionCodeHandling::Keep);
    }

    /// The slot convention is V8's — the first added context is slot 1 — and a
    /// blob names every context the creator recorded, empty ones included.
    #[test]
    fn an_added_context_gets_slot_one() {
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
        // The added context was recorded, so its slot is named: what it holds is
        // nothing, which is not the same as the blob not naming it.
        assert!(restored_context(&mut isolate, 1).is_some());
        assert!(
            restored_context(&mut isolate, 2).is_none(),
            "a slot no context was recorded for"
        );
    }

    /// Data attached to a context the creator added comes back at that
    /// context's slot, in its own realm. This is the shape `deno_core` builds: an
    /// empty default context at slot 0 and its bootstrapped realm at slot 1,
    /// with everything it attached on the realm.
    #[test]
    fn an_added_context_carries_its_own_data() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let default = Context::new(scope, Default::default());
            let added = Context::new(scope, Default::default());
            scope.set_default_context(default);
            assert_eq!(scope.add_context(added), 1);
            let zero: Local<Value> = Number::new(scope, 0.0).into();
            scope.add_context_data(default, zero);
            // A value built in the added context's realm, which is what a host's
            // bootstrap leaves behind there.
            let scope = &mut crate::ContextScope::new(scope, added);
            let object = crate::test_support::eval(scope, "({ n: 7 })");
            scope.add_context_data(added, object);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob.clone());
        assert_eq!(
            data(&mut isolate, 1),
            vec![Some(0.0)],
            "slot 0 is the default context's own data"
        );

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, 1).expect("the blob names slot 1");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let object = scope
            .get_context_data_from_snapshot_once::<Object>(0)
            .expect("the added context's object");
        let key = crate::test_support::eval(scope, "'n'");
        let n = object.get(scope, key).expect("the property");
        assert_eq!(
            Local::<Number>::try_from(n)
                .ok()
                .map(|number| number.value()),
            Some(7.0),
            "the object came back with what it held"
        );
    }

    /// Data attached to a context the snapshot does not carry is refused where
    /// the host attaches it, which is where it can act on the refusal.
    #[test]
    #[should_panic(expected = "not one this snapshot carries")]
    fn data_for_an_unrecorded_context_is_refused() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        crate::scope!(let scope, &mut isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut crate::ContextScope::new(scope, context);
        scope.set_default_context(context);
        let stranger = Context::new(scope, Default::default());
        let number: Local<Value> = Number::new(scope, 1.0).into();
        scope.add_context_data(stranger, number);
    }

    /// A context no default was set for and no `add_context` recorded is not one
    /// the blob carries either, and says so the same way.
    #[test]
    #[should_panic(expected = "not one this snapshot carries")]
    fn data_without_a_recorded_context_is_refused() {
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

    /// A host pointer round trips through the host's table: the creator writes
    /// the index, and the restored isolate resolves it against the table it was
    /// built with — the addresses are the process's, not the blob's.
    #[test]
    fn a_host_pointer_round_trips_through_the_table() {
        let pointer = 0x5150usize as *mut std::ffi::c_void;
        let other = 0x8888usize as *mut std::ffi::c_void;
        let mut isolate = Isolate::snapshot_creator(Some(references(&[other, pointer])), None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let external = crate::External::new(scope, pointer);
            let value: Local<Value> = external.into();
            scope.add_context_data(context, value);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        // The same table, rebuilt for this load, which is the host's contract.
        let mut isolate = Isolate::new(
            crate::CreateParams::default()
                .snapshot_blob(blob)
                .external_references(references(&[other, pointer])),
        );
        let context = restored_context(&mut isolate, 0).expect("the blob names slot 0");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let back = scope
            .get_context_data_from_snapshot_once::<crate::External>(0)
            .expect("the external");
        assert_eq!(back.value(), pointer);
    }

    /// A pointer the creator's table does not have ends the build rather than
    /// writing an index nothing would resolve.
    #[test]
    #[should_panic(expected = "external-reference table")]
    fn a_host_pointer_not_in_the_creators_table_ends_the_build() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let external = crate::External::new(scope, 0x1234usize as *mut std::ffi::c_void);
            let value: Local<Value> = external.into();
            scope.add_context_data(context, value);
        }
        let _ = isolate.create_blob(FunctionCodeHandling::Keep);
    }

    /// A blob whose header checked out but whose body cannot be read for the
    /// table this isolate has is a host contract violation, and says so rather
    /// than sending the host down its "boot from source" branch.
    #[test]
    #[should_panic(expected = "could not be read for context")]
    fn a_table_the_blob_does_not_match_ends_the_restore() {
        let pointer = 0x4242usize as *mut std::ffi::c_void;
        let mut isolate = Isolate::snapshot_creator(Some(references(&[pointer])), None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let external = crate::External::new(scope, pointer);
            let value: Local<Value> = external.into();
            scope.add_context_data(context, value);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        // Loaded with a table that never had the address the blob names.
        let mut isolate = Isolate::new(crate::CreateParams::default().snapshot_blob(blob));
        let _ = restored_context(&mut isolate, 0);
    }

    /// The table form of the reference list a creator or an isolate takes.
    fn references(
        pointers: &[*mut std::ffi::c_void],
    ) -> std::borrow::Cow<'static, [crate::ExternalReference]> {
        std::borrow::Cow::Owned(
            pointers
                .iter()
                .map(|pointer| crate::ExternalReference { pointer: *pointer })
                .collect(),
        )
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
