//! Snapshot data a host carries between runs (`v8::StartupData`), and the
//! creator it would come from (`v8::SnapshotCreator`).
//!
//! Slag has no snapshot format: an isolate boots from source, which is what a
//! snapshot is an optimization of. So a blob a host hands to
//! [`CreateParams::snapshot_blob`](crate::CreateParams::snapshot_blob) is
//! carried, readable and storable, and not consumed — the contents are never
//! read back into a heap — and
//! [`SnapshotCreator::create_blob`](OwnedIsolate::create_blob) cannot produce
//! one at all. Both ends of the round trip say so: `is_valid` answers `false` for
//! a blob, and creating one aborts with the reason rather than handing back
//! something nothing could load. A host that depends on its snapshot's contents
//! (state its extensions installed at build time) must be built without one,
//! which is a configuration a Deno-class host already has.

use std::ops::Deref;

use runtime::api;

use crate::isolate::{Isolate, OwnedIsolate};

/// Serialized engine state a host carries between runs (v8::StartupData).
#[derive(Debug, Clone)]
pub struct StartupData(std::borrow::Cow<'static, [u8]>);

impl StartupData {
    /// Whether the data could be rehashed on deserialization
    /// (v8::StartupData::CanBeRehashed).
    ///
    /// Only data a `SnapshotCreator` produced could be; this bridge produces
    /// none, so the answer describes data it can use: none.
    pub fn can_be_rehashed(&self) -> bool {
        false
    }

    /// Whether the data is valid for this engine
    /// (v8::StartupData::IsValid).
    ///
    /// A blob from V8 is not valid for Slag, and Slag produces none of its own,
    /// so this is `false`. A host that checks it — the crate we stand in for
    /// expects one to — is told the blob cannot be used rather than handed a
    /// boot that silently ignores it.
    pub fn is_valid(&self) -> bool {
        false
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
/// It records what the crate we stand in for would put in a blob — the context
/// a deserialized isolate starts in, the contexts added, the index of each piece
/// of data attached — and [`create_blob`](Self::create_blob) refuses, because
/// there is no format to serialize a heap into. Recording is what keeps the
/// indices a host stores true numbers: they are the positions it asked for, not
/// invented ones.
#[derive(Default)]
pub(crate) struct SnapshotCreator {
    /// The context a deserialized isolate would start in.
    default_context: Option<api::Context>,
    /// The contexts added, in the order they were added.
    contexts: Vec<api::Context>,
    /// How many pieces of data have been attached to the context snapshot.
    context_data: usize,
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
    pub(crate) fn add_context(&mut self, context: api::Context) -> usize {
        self.contexts.push(context);
        self.contexts.len() - 1
    }

    /// Record data attached to the context snapshot, answering the index it was
    /// given (v8::SnapshotCreator::AddContextData).
    ///
    /// The data itself is not held: nothing can read it back, because no blob is
    /// ever produced ([`create_blob`](Self::create_blob)), so holding a handle to
    /// it would only keep a heap box alive for a lookup that cannot happen.
    pub(crate) fn add_context_data(&mut self) -> usize {
        self.context_data += 1;
        self.context_data - 1
    }

    /// Refuse to produce a blob.
    ///
    /// The crate we stand in for serializes the isolate's heap here, which is
    /// what lets a host skip its own bootstrap. Slag boots from source, so there
    /// is nothing to serialize and nothing that could read a blob back. Its
    /// callers answer `Option` and unwrap it, so aborting with the reason is the
    /// loudest thing this signature allows — and the alternative, a blob that
    /// boots nothing, would be a silent one.
    pub(crate) fn create_blob(
        &mut self,
        _function_code_handling: FunctionCodeHandling,
    ) -> Option<StartupData> {
        panic!(
            "Slag has no snapshot format: an isolate boots from source, so v8::SnapshotCreator cannot produce a blob (see v8::StartupData::is_valid)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Context;
    use crate::Local;
    use crate::data::Data;

    /// A creator isolate is a real isolate: a context can be created in it, the
    /// default context and added contexts are recorded, and each attached piece
    /// of data gets the index a host stores.
    #[test]
    fn a_snapshot_creator_records_what_a_blob_would_carry() {
        let isolate = &mut Isolate::snapshot_creator(None, None);
        crate::scope!(let scope, isolate);
        let context = Context::new(scope, Default::default());
        let default_context = Context::new(scope, Default::default());
        let second = Context::new(scope, Default::default());
        let scope = &mut crate::ContextScope::new(scope, context);

        // The isolate is reached through the scope, which is how a host's op
        // entry point reaches it.
        scope.set_default_context(default_context);
        assert_eq!(scope.add_context(second), 0);
        assert_eq!(scope.add_context(second), 1);

        // And the data a host attaches arrives as a `Data` handle, which is the
        // shape `deno_core` passes.
        let data: Local<'_, Data> = default_context.into();
        assert_eq!(scope.add_context_data(context, data), 0);
        assert_eq!(scope.add_context_data(context, data), 1);

        // And it is usable as an isolate, which is what makes the recorded state
        // worth recording.
        assert_eq!(crate::test_support::eval_number(scope, "6 * 7"), 42.0);
    }

    /// The creator constructors take exactly what the crate we stand in for's
    /// take, including external references and create params.
    #[test]
    fn a_creator_takes_external_references_and_create_params() {
        let references: std::borrow::Cow<'static, [crate::ExternalReference]> =
            std::borrow::Cow::Owned(Vec::new());
        let isolate =
            &mut Isolate::snapshot_creator(Some(references), Some(crate::CreateParams::default()));
        crate::scope!(let scope, isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut crate::ContextScope::new(scope, context);
        assert_eq!(crate::test_support::eval_number(scope, "2 * 21"), 42.0);
    }

    /// The two ends of the round trip both refuse: a blob cannot be produced,
    /// and an isolate that is not a creator has nothing to serialize.
    #[test]
    #[should_panic(expected = "Slag has no snapshot format")]
    fn creating_a_blob_refuses_with_the_reason() {
        let isolate = Isolate::snapshot_creator(None, None);
        let _ = isolate.create_blob(FunctionCodeHandling::Keep);
    }

    #[test]
    #[should_panic(expected = "not created by Isolate::snapshot_creator")]
    fn setting_a_default_context_needs_a_creator_isolate() {
        let isolate = &mut Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, isolate);
        let context = Context::new(scope, Default::default());
        scope.set_default_context(context);
    }

    /// A blob a host carries is readable and invalid, which is the answer that
    /// tells it to boot without one.
    #[test]
    fn a_carried_blob_is_readable_and_not_valid() {
        let data = StartupData::from(vec![7u8, 8, 9]);
        assert_eq!(&*data, &[7, 8, 9]);
        assert!(!data.is_valid());
        assert!(!data.can_be_rehashed());

        // And a creator that continues from it is still a real isolate.
        let isolate = &mut Isolate::snapshot_creator_from_existing_snapshot(data, None, None);
        crate::scope!(let scope, isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut crate::ContextScope::new(scope, context);
        assert_eq!(crate::test_support::eval_number(scope, "1 + 1"), 2.0);
    }
}
