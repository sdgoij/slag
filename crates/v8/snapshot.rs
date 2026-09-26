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
//! An attached item is a language value or a module record, and a blob carries
//! every one of them: a load hands a slot's items back by *index*, so an item
//! that were left out would move every later one to an earlier number than the
//! host stored it under.
//!
//! A JavaScript function is carried as the source text it can be re-parsed from,
//! so a restored function is callable and a host's attached callbacks come back.
//! A class constructor is one of those, with its class as the source — so a
//! restored class is rebuilt by the engine's own class evaluation, which means
//! its definition-time code (a computed key, a `static {}` block, a static field
//! initializer) runs again at restore, where V8's snapshot restores objects.
//! What the source cannot say is the scope the function closed over: a restored
//! function resolves a free name through the realm's global environment.
//!
//! A **host callback** — a Rust closure the host built through
//! `FunctionTemplate`/`Function::builder`, which is every Deno op — has no source
//! at all, so it is carried as the two things only the host has: the index of its
//! external-reference table entry the call came from, and the data value it was
//! built with (`.data(...)`, which its callback reads). A blob never holds the
//! address; the table the *loading* host rebuilds names the call again, which is
//! the same reason a host pointer is an index here.
//!
//! # Index conventions
//!
//! V8's, because the host this stands in for reads them back: the *added*
//! contexts own the index space, numbered from 0 in the order they were added,
//! and `AddContext` answers the index it gave one. The default context — the one
//! a deserialized isolate starts in — is **not** in that space: a host reaches it
//! as the startup context, not by `Context::FromSnapshot`, so a blob carries its
//! data at the reserved [`DEFAULT_CONTEXT_SLOT`]. `deno_core` is the shape this
//! serves: an embedder's node:vm context takes 0 and its own main realm takes 1,
//! and it reads the realm back with `from_snapshot(1)` before falling back to
//! `from_snapshot(0)` ("embedder may have used 0th for something else"). A
//! context's own data starts at 0.
//! A blob records a slot for every context the creator knows, holding
//! `undefined` for one that was never recorded, so a slot the host never used
//! answers "the blob names no such context" rather than "that context was
//! empty".
//!
//! # What is not carried yet
//!
//! A value the engine's walk refuses — a proxy, a typed array, a module
//! namespace, a host object, an array with a hole — ends
//! [`create_blob`](SnapshotCreator::create_blob) with a panic naming it, and so
//! does a host callback the host's external-reference table does not hold, or one
//! the host built constructible. A **function template** attached as context data
//! is carried as the record [`template_record`] writes; a template carrying a part
//! that record does not hold yet — a property of its own, an instance or prototype
//! template, a parent — is refused where the host attaches it, naming the part.
//! The crate's signature is `Option`, which its own
//! callers unwrap,
//! so the loudest available message is the honest one; a blob that quietly lost
//! part of a host's state would move that failure to where the host cannot see
//! it. An attached item that is neither — a context, a script, a stack frame — is
//! refused where the host attaches it, for the same reason.
//!
//! Neither `FunctionCodeHandling` mode carries compiled code, because a blob
//! holds the source a function is rebuilt from rather than a compiled body:
//! both answers write the same blob. Isolate-level data and
//! continuation-from-an-existing-blob are likewise not carried yet, and say so
//! where a host would look for them.

use std::collections::HashMap;
use std::ops::Deref;
use std::rc::Rc;

use runtime::api;
use runtime::snapshot as format;

use crate::data::Data;
use crate::handle::{Global, Local, Payload};
use crate::isolate::{BuiltCallback, Isolate, OwnedIsolate};
use format::HostCallbacks as _;

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
/// The slot a blob carries the default context's data at — the context a
/// deserialized isolate starts in.
///
/// It is deliberately outside any index a host can name. V8's *added* contexts
/// own the index space (0, 1, ...) and the default context is not one of them, so
/// giving it slot 0 would collide with the first added context — which is exactly
/// what deno's node:vm context is. The reserved slot keeps the default's attached
/// data restorable by the bridge's own load without taking an index a host means;
/// it is the largest index the blob's 32-bit slot field can carry, which no host
/// reaches by adding contexts.
pub(crate) const DEFAULT_CONTEXT_SLOT: usize = u32::MAX as usize;

#[derive(Default)]
pub(crate) struct SnapshotCreator {
    /// The context a deserialized isolate starts in, carried at
    /// [`DEFAULT_CONTEXT_SLOT`].
    default_context: Option<api::Context>,
    /// The contexts added after it, each with the index `add_context` answered
    /// and the blob carries it under: 0, 1, ...
    contexts: Vec<(usize, api::Context)>,
    /// The data attached to each context, by the slot that context has in the
    /// blob, in the order it was attached: the index a restore hands each one
    /// back under.
    attached: HashMap<usize, Vec<Attached>>,
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
    /// The added contexts own the index space, from 0 in the order they are
    /// added — which is what a host reads back with `Context::FromSnapshot`. The
    /// default context is not in that space (see [`DEFAULT_CONTEXT_SLOT`]), so
    /// setting one does not shift these indices.
    pub(crate) fn add_context(&mut self, context: api::Context) -> usize {
        let index = self.contexts.len();
        self.contexts.push((index, context));
        index
    }

    /// Record data attached to `context`, answering the index it was given
    /// (v8::SnapshotCreator::AddContextData).
    ///
    /// # Panics
    ///
    /// Panics when `context` is not one this snapshot carries — the statement
    /// V8's own check makes: data attached to a context a blob does not name
    /// would have nowhere to be read back out of. Panics too when the data is
    /// something a blob has no item for, which is decided here rather than when
    /// the blob is written: only the host knows what it attached, and a slot the
    /// blob held fewer items for would move every later attach to an earlier
    /// index. Panics too when the isolate did not come from
    /// `Isolate::snapshot_creator`.
    pub(crate) fn add_context_data(&mut self, context: api::Context, data: Global<Data>) -> usize {
        let slot = self.slot_of(context).unwrap_or_else(|| {
            panic!(
                "v8::Isolate::AddContextData: the context is not one this snapshot carries: call set_default_context or add_context for it first"
            )
        });
        let index = self.attached.get(&slot).map_or(0, Vec::len);
        let item = match data.payload_value() {
            Payload::Value(value) => format::SnapshotItem::Value(*value.value()),
            Payload::Module(module) => format::SnapshotItem::Module(module),
            other => panic!(
                "v8::Isolate::AddContextData: the data at slot {slot}, index {index} is {}; a snapshot carries values and module records",
                other.kind()
            ),
        };
        let items = self.attached.entry(slot).or_default();
        items.push(Attached { item, held: data });
        items.len() - 1
    }

    /// The slot a context's data is carried in, if this snapshot carries it:
    /// [`DEFAULT_CONTEXT_SLOT`] for the default context, else the index
    /// `add_context` answered for it.
    fn slot_of(&self, context: api::Context) -> Option<usize> {
        let key = context_identity(context);
        if self
            .default_context
            .is_some_and(|default| context_identity(default) == key)
        {
            return Some(DEFAULT_CONTEXT_SLOT);
        }
        self.contexts
            .iter()
            .find(|(_, added)| context_identity(*added) == key)
            .map(|(index, _)| *index)
    }

    /// Write the blob.
    ///
    /// The graph is the context table: each added context at the index
    /// `add_context` answered, and the default context (if one was set) at
    /// [`DEFAULT_CONTEXT_SLOT`]. A value the engine cannot carry ends this with a
    /// panic naming it — see the module docs for why that is the loudest message
    /// this signature allows.
    pub(crate) fn create_blob(
        &mut self,
        function_code_handling: FunctionCodeHandling,
        callbacks: HashMap<u64, BuiltCallback>,
    ) -> StartupData {
        // Neither mode carries compiled code: a function is carried as the
        // source it is rebuilt from, so a blob is the same either way.
        let _ = function_code_handling;
        assert!(
            self.default_context.is_some() || !self.contexts.is_empty(),
            "v8::SnapshotCreator::create_blob: a snapshot carries what a context holds, so the creator needs one"
        );
        let mut slots: Vec<(usize, api::Context, Vec<format::SnapshotItem>)> = Vec::new();
        if let Some(default) = self.default_context {
            slots.push((
                DEFAULT_CONTEXT_SLOT,
                default,
                self.items_of(DEFAULT_CONTEXT_SLOT),
            ));
        }
        for (index, context) in &self.contexts {
            slots.push((*index, *context, self.items_of(*index)));
        }
        let host = SnapshotCallbacks {
            callbacks,
            record_into: None,
        };
        // The realm's global state travels too: a host's snapshot is meant to put
        // back what it installed on the global (`Deno`, an op table), which is
        // what `InitMode::FromSnapshot` reads before it runs any bootstrap.
        match api::Context::write_snapshot(
            &slots,
            &engine_table(&self.externals),
            Some(&host),
            true,
        ) {
            Ok(bytes) => StartupData::new(bytes),
            Err(error) => {
                panic!("v8::SnapshotCreator::create_blob: the engine cannot carry {error} yet")
            }
        }
    }

    /// The items attached to one slot, in the order they were attached.
    fn items_of(&self, slot: usize) -> Vec<format::SnapshotItem> {
        self.attached
            .get(&slot)
            .map(|items| items.iter().map(|held| held.item).collect())
            .unwrap_or_default()
    }
}

/// One item a host attached, with the handle that keeps it alive until the blob
/// is written.
///
/// The item is read out of the handle at attach time rather than when the blob is
/// written, because that is where a handle a blob has no item for is refused (see
/// [`SnapshotCreator::add_context_data`]).
struct Attached {
    item: format::SnapshotItem,
    /// Held only for its pin: the blob reads the value this names, and a value in
    /// host memory is not a root, so without this the collector could sweep it
    /// before the blob is written.
    #[allow(dead_code)]
    held: Global<Data>,
}

/// This bridge as the engine's [`HostCallbacks`](format::HostCallbacks).
///
/// One struct for both halves, because they are one fact about the host: a
/// function the host built keeps the callback pointer its template was built
/// from, and the table the host rebuilds for a load names the call again. The
/// write side reads the isolate's table of those; the read side needs nothing of
/// its own, because the call view the engine hands a callback already carries
/// the isolate it runs on.
struct SnapshotCallbacks {
    callbacks: HashMap<u64, BuiltCallback>,
    /// The isolate a *load* records what it rebuilt into, so a snapshot written
    /// of that load can name it. `None` on the write side, which was handed the
    /// table it writes against.
    record_into: Option<Isolate>,
}

impl SnapshotCallbacks {
    /// The read half, which answers for any entry the load's table holds and
    /// records the functions it rebuilds into `isolate` — the identity a second
    /// snapshot asks with.
    fn for_load(isolate: Isolate) -> Self {
        Self {
            callbacks: HashMap::new(),
            record_into: Some(isolate),
        }
    }
}

impl format::HostCallbacks for SnapshotCallbacks {
    fn callback_of(
        &self,
        function: &crux::handle::Handle<crux::function::Function>,
    ) -> Option<format::HostCallback> {
        let recorded = self.callbacks.get(&function.id())?;
        Some(format::HostCallback {
            pointer: recorded.callback,
            // The pinned value, as the engine's own handle: `Global` keeps it a
            // root for as long as this table holds it, which is what makes the
            // value the walk reads the one the function was built with.
            data: recorded.data.as_ref().map(|pinned| pinned.engine_value()),
        })
    }

    fn callback_rebuilt(
        &self,
        function: &crux::handle::Handle<crux::function::Function>,
        pointer: usize,
        data: Option<api::Local>,
    ) {
        if let Some(isolate) = self.record_into {
            isolate.record_rebuilt_callback(function.id(), pointer, data);
        }
    }

    fn callback_at(
        &self,
        pointer: usize,
        data: Option<api::Local>,
    ) -> Option<api::FunctionCallback> {
        // A table entry the host filled with anything but a callback — the
        // `nullptr` entry V8's tables conventionally end with, for one — cannot
        // be one of these, and the record can only name an entry the write side
        // recorded a callback for.
        if pointer == 0 {
            return None;
        }
        // SAFETY: the entry is one the host put in its own table, whose
        // `function` field is a `FunctionCallback` — the pointer the host's
        // callback had when the function the record names was built, and the
        // same address this bridge calls it through when it makes a function
        // from a template.
        let callback: crate::function::FunctionCallback = unsafe { std::mem::transmute(pointer) };
        Some(Box::new(move |info: &api::FunctionCallbackInfo<'_>| {
            // The data the blob carried, which is what the callback reads through
            // `FunctionCallbackArguments::data` — `undefined` for one built
            // without any, exactly as the template path hands it.
            let data = match data {
                Some(data) => Local::from_engine(data),
                None => Local::from_engine(api::Local::undefined()),
            };
            let view = crate::function::FunctionCallbackInfo::new(info, data);
            // SAFETY: the engine hands a callback a view it keeps alive for the
            // call, and this closure only reads it here.
            unsafe { callback(&view) };
        }))
    }
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
        .map(|reference| unsafe { reference.pointer })
        .collect()
}

/// The table the engine writes and reads a snapshot's indices against: **this
/// bridge's own callbacks first, then the host's**.
///
/// A host's table is the host's to rebuild for every load, and the two builds are
/// legitimately different lengths: deno externalizes its lazy sources only while
/// snapshotting, so the table it hands the creator is not the table it hands the
/// loader. That is why the bridge's entries come first rather than last — a
/// position that depends on a host's table length would name one entry when the
/// blob was written and another when it is read.
///
/// What goes in is what the *bridge* installs and the host cannot know about: the
/// console methods, which are one callback under many names
/// ([`console_callback`](crate::context::console_callback)), and the
/// continuation-preserved embedder data accessors
/// ([`get_continuation_data_callback`](crate::context::get_continuation_data_callback)),
/// all built for every context this bridge makes — including the ones a snapshot
/// is taken of, whose global object the walk reaches.
fn engine_table(references: &[crate::ExternalReference]) -> Vec<*mut std::ffi::c_void> {
    let bridge = [
        crate::context::console_callback(),
        crate::context::get_continuation_data_callback(),
        crate::context::set_continuation_data_callback(),
    ];
    let mut table = Vec::with_capacity(references.len() + bridge.len());
    table.extend(bridge.map(|callback| callback as *mut std::ffi::c_void));
    table.extend_from_slice(&addresses(references));
    table
}

/// The key a function template's record carries its own brand under.
///
/// A template has no engine object of its own, so the record has to be
/// recognizable as *this bridge's* and not as a value a host attached. The key is
/// a name no host would write, and strings compare by content — so the brand
/// travels through a blob and comes back as itself.
const TEMPLATE_RECORD: &str = "\u{1}slag:function-template";
const TEMPLATE_CALLBACK: &str = "callback";
const TEMPLATE_DATA: &str = "data";
const TEMPLATE_LENGTH: &str = "length";
const TEMPLATE_CONSTRUCTIBLE: &str = "constructible";
const TEMPLATE_NAME: &str = "name";
const TEMPLATE_PARENT: &str = "parent";

/// The pointer an `External` value carries, or `None` for a value that is not
/// one — which is how this bridge recognizes its own template handles, whose
/// payload is an `External` naming the template's address.
fn external_pointer(value: &api::Local) -> Option<*mut std::ffi::c_void> {
    let object = value.value().as_object()?;
    match &object.kind {
        crux::object::ObjectKind::External(pointer) => Some(*pointer as *mut std::ffi::c_void),
        _ => None,
    }
}

/// The record a function-template item is carried as, or `None` when the item is
/// not one of this isolate's templates — which the engine then carries as the
/// value it is.
///
/// A template's handle is an `External` naming the address the isolate took the
/// `Rc<FunctionTemplate>` under, and an address means nothing to the process that
/// reads a blob. So the item is written as what a template can be rebuilt from:
/// its callback — as a host *pointer*, which the engine already carries as an
/// index into the external-reference table both ends rebuild — the data it was
/// built with, its `length`, whether it is constructible, and its class name.
/// The load makes a new template under a fresh address, so the restored handle
/// names this process's own.
///
/// # Panics
///
/// Panics when the template carries a part this record does not hold yet, naming
/// it. A record that dropped one would come back as a template that is not the one
/// that was written, which is the failure this bridge refuses everywhere else; a
/// host attaches its data before it writes a blob, so this is where it is told.
pub(crate) fn template_record<'s>(
    isolate: &Isolate,
    context: api::Context,
    data: Local<'s, Data>,
) -> Option<Local<'s, Data>> {
    let pointer = external_pointer(data.engine())?;
    if !isolate.owns_template(pointer) {
        return None;
    }
    // SAFETY: `owns_template` said this isolate holds a template at the address,
    // and the isolate outlives every handle on it.
    let template = unsafe { &*(pointer as *const api::FunctionTemplate) };
    let record = template_record_of(isolate, context, pointer as usize, template)?;
    Some(Local::from_engine(record))
}

/// The record for one template, by the address its handle names — the recursion
/// [`template_record`] starts, and the one a template's parent goes through.
///
/// The record object is made and remembered **before** its properties are filled,
/// so a template that inherits from one already being written — or from itself —
/// names the record under construction. One object per template is what the load
/// needs to come back with one template per record: the engine writes a value
/// once, so the object two children name is the object they share.
fn template_record_of(
    isolate: &Isolate,
    context: api::Context,
    address: usize,
    template: &api::FunctionTemplate,
) -> Option<api::Local> {
    let identity = context_identity(context);
    if let Some(record) = isolate.template_record_for(identity, address) {
        return Some(record.engine_value());
    }
    if template.property_count() != 0 {
        panic!(
            "v8::Isolate::AddContextData: this function template cannot be carried yet: a property of its own"
        );
    }
    if template.has_instance_template() || template.has_prototype_template() {
        panic!(
            "v8::Isolate::AddContextData: this function template cannot be carried yet: an instance or prototype template"
        );
    }
    let (callback, attached) = isolate.template_parts(address)?;
    let record = api::Object::new(&context).expect("bridge: a template record object");
    isolate.remember_template_record(
        identity,
        address,
        Global::new(isolate, Local::<Data>::from_engine(record)),
    );
    let set = |key: &str, value: api::Local| {
        api::Object::set(&context, &record, key, &value, false)
            .expect("bridge: a template record property");
    };
    set(TEMPLATE_RECORD, api::Local::boolean(true));
    set(
        TEMPLATE_CALLBACK,
        api::Local::from(crux::value::Value::Object(
            crux::object::JsObject::external_object_create(callback, None),
        )),
    );
    if let Some(attached) = attached {
        set(TEMPLATE_DATA, attached.engine_value());
    }
    set(
        TEMPLATE_LENGTH,
        api::Local::number(f64::from(template.length())),
    );
    set(
        TEMPLATE_CONSTRUCTIBLE,
        api::Local::boolean(template.constructible()),
    );
    if let Some(name) = template.class_name() {
        set(TEMPLATE_NAME, api::Local::string(name));
    }
    if let Some(parent) = template.parent() {
        let parent = template_record_of(isolate, context, Rc::as_ptr(&parent) as usize, &parent)?;
        set(TEMPLATE_PARENT, parent);
    }
    Some(record)
}

/// The function template a record names, taken under this isolate — or `None` for
/// a value that is not a record, which is every other item a slot holds.
///
/// The template made here is the *load's*: its callback comes from the load's own
/// external-reference table (the same entry the write side named), its data is the
/// value the blob carried, and the isolate takes it under the address a handle in
/// this process names. It is recorded in `template_callback` as well, so a snapshot
/// *of this load* names it again rather than refusing it — which is what deno's
/// two-pass creation needs.
fn template_from_record(
    isolate: &mut Isolate,
    context: api::Context,
    value: crux::value::Value,
    rebuilt: &mut HashMap<u64, Rc<api::FunctionTemplate>>,
) -> Option<api::Local> {
    let value = api::Local::from(value);
    if !api::Object::has_own_key(&context, &value, &api::Local::string(TEMPLATE_RECORD)).ok()? {
        return None;
    }
    let template = rebuild_template(isolate, context, &value, rebuilt)?;
    Some(api::Local::from(crux::value::Value::Object(
        crux::object::JsObject::external_object_create(Rc::as_ptr(&template) as usize, None),
    )))
}

/// The template one record is, made **once per record**: the recursion a template's
/// parent goes through, and what makes two children that named one record share the
/// parent they come back with.
///
/// The made template is remembered before its parent is taken, so a chain that
/// comes back to this record — a template that inherits from itself — names this
/// template rather than starting a second one.
fn rebuild_template(
    isolate: &mut Isolate,
    context: api::Context,
    record: &api::Local,
    rebuilt: &mut HashMap<u64, Rc<api::FunctionTemplate>>,
) -> Option<Rc<api::FunctionTemplate>> {
    let identity = record.value().as_object()?.id();
    if let Some(template) = rebuilt.get(&identity) {
        return Some(Rc::clone(template));
    }
    let callback = api::Object::get(&context, record, TEMPLATE_CALLBACK).ok()?;
    let callback_pointer = external_pointer(&callback)?;
    let attached = api::Object::has_own_key(&context, record, &api::Local::string(TEMPLATE_DATA))
        .ok()?
        .then(|| api::Object::get(&context, record, TEMPLATE_DATA).ok())
        .flatten();
    let length = api::Object::get(&context, record, TEMPLATE_LENGTH)
        .ok()?
        .value()
        .as_number()?;
    let constructible = api::Object::get(&context, record, TEMPLATE_CONSTRUCTIBLE)
        .ok()?
        .value()
        .as_boolean()?;
    let name = api::Object::get(&context, record, TEMPLATE_NAME)
        .ok()
        .and_then(|name| name.value().as_string())
        .map(|name| name.to_string_lossy());
    // The callback is the one the load's table holds at that address — the host
    // rebuilt the entry, and this is what resolves it back into a call.
    let host = SnapshotCallbacks::for_load(*isolate);
    let callback = host.callback_at(callback_pointer as usize, attached)?;
    let template = api::FunctionTemplate::new(isolate.engine_mut(), callback);
    template.set_length(length as i32);
    template.set_constructible(constructible);
    if let Some(name) = name {
        template.set_class_name(&name);
    }
    let address = Rc::as_ptr(&template) as usize;
    isolate.add_template(Rc::clone(&template));
    let attached = attached
        .map(|attached| Global::new(isolate, Local::<crate::data::Value>::from_engine(attached)));
    isolate.template_callback(address, callback_pointer as usize, attached);
    rebuilt.insert(identity, Rc::clone(&template));
    if api::Object::has_own_key(&context, record, &api::Local::string(TEMPLATE_PARENT)).ok()? {
        let parent = api::Object::get(&context, record, TEMPLATE_PARENT).ok()?;
        let parent = rebuild_template(isolate, context, &parent, rebuilt)?;
        template.inherit(&parent);
    }
    Some(template)
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
    fn restore(&mut self, isolate: &mut Isolate, context: api::Context, slot: usize) -> bool {
        let table = engine_table(isolate.externals());
        let host = SnapshotCallbacks::for_load(*isolate);
        let items = match context.read_snapshot(self.blob.bytes(), slot, &table, Some(&host)) {
            Ok(Some(items)) => items,
            Ok(None) => return false,
            Err(error) => panic!(
                "v8::Context::FromSnapshot: the snapshot could not be read for context {slot}: {error}"
            ),
        };
        let mut held = Vec::with_capacity(items.len());
        // One template per record, for the items of this slot: two templates that
        // shared a parent named one record for it, and this is what makes them
        // share the parent they come back with.
        let mut rebuilt = HashMap::new();
        for item in items {
            let payload = match item {
                format::SnapshotItem::Value(value) => {
                    // A function template is not a value the engine carried: the
                    // record names what one is made of, and the template is made
                    // here — under this process's own address — before the host
                    // asks for it.
                    match template_from_record(isolate, context, value, &mut rebuilt) {
                        Some(handle) => Payload::Value(handle),
                        None => Payload::Value(value.into()),
                    }
                }
                format::SnapshotItem::Module(module) => Payload::Module(module),
            };
            let handle: Local<'_, Data> = Local::from_payload(payload);
            held.push(Some(Global::new(isolate, handle)));
        }
        self.items.insert(context_identity(context), held);
        true
    }

    /// Whether the blob's context table names `slot`, answered from the table
    /// alone — no realm is made for it. `None` when the table cannot be walked
    /// at all: the caller then makes the realm and lets [`Self::restore`] name
    /// the real reason.
    fn names_slot(&self, slot: usize) -> Option<bool> {
        format::names_slot(self.blob.bytes(), slot).ok()
    }

    /// The item at `index` of a restored context's data, taken.
    fn take(&mut self, context_identity: u64, index: usize) -> Option<Global<Data>> {
        self.items
            .get_mut(&context_identity)?
            .get_mut(index)?
            .take()
    }
}

/// Whether the isolate's blob names `slot`, answered without making a realm —
/// `false` for an isolate that booted from source, which has no blob to name
/// one.
///
/// The restore is taken out and put back for the borrow, the way
/// [`restore_context`] does it. A table that cannot be walked is answered
/// `true`, so the realm is made and `restore` reports why rather than a corrupt
/// blob reading as a missing slot.
pub(crate) fn blob_names_slot(isolate: &mut Isolate, slot: usize) -> bool {
    let Some(restore) = isolate.take_restore() else {
        return false;
    };
    let present = restore.names_slot(slot);
    isolate.put_restore(restore);
    present.unwrap_or(true)
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
    use crate::{Context, ContextOptions, Global, Isolate, Local, Module, Object, OwnedIsolate};

    /// An isolate booted from `blob`.
    fn isolate_from(blob: StartupData) -> OwnedIsolate {
        Isolate::new(crate::CreateParams::default().snapshot_blob(blob))
    }

    /// The same, with the external-reference table a host rebuilds for every
    /// load: the blob carries indices into it, so a restore without it answers
    /// for none of them.
    fn isolate_from_with(
        blob: StartupData,
        references: Vec<crate::ExternalReference>,
    ) -> OwnedIsolate {
        Isolate::new(
            crate::CreateParams::default()
                .snapshot_blob(blob)
                .external_references(std::borrow::Cow::Owned(references)),
        )
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
        let context = restored_context(isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
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

    /// A slot the blob does not name is answered without making a realm. A realm
    /// made and passed over is still registered on the isolate and still counts,
    /// and more than one realm is what takes every call in the isolate off the
    /// engine's fast path — so the probe itself is what has to be free.
    #[test]
    fn a_slot_the_blob_does_not_name_costs_no_realm() {
        let mut isolate = isolate_from(numbers_blob(&[7.0]));
        let realms = |isolate: &mut OwnedIsolate| {
            crate::scope!(let scope, isolate);
            scope.get_heap_statistics().number_of_native_contexts()
        };

        // The probe `deno_core` makes, twice: a slot below the one the blob
        // carries and one above it.
        assert!(restored_context(&mut isolate, 0).is_none());
        let after_miss = realms(&mut isolate);
        assert!(restored_context(&mut isolate, 5).is_none());
        assert_eq!(realms(&mut isolate), after_miss, "a miss makes no realm");

        // And the hit that follows still restores, in exactly one realm.
        let _restored = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        assert_eq!(
            realms(&mut isolate),
            after_miss + 1,
            "the restored realm, and none from either miss"
        );
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

    /// What a host installed on its global survives the blob, and a property that
    /// held the writing realm's global holds the **reading** realm's after the
    /// restore — the identity `globalThis` is built on. This is the state
    /// `deno_core`'s `InitMode::FromSnapshot` reads (`Deno`, the op table) before
    /// it runs any bootstrap, so a blob without it cannot restore a host at all.
    #[test]
    fn a_hosts_installed_global_survives_the_blob() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            crate::test_support::eval(
                scope,
                "globalThis.__carried = { n: 7 }; globalThis.__self = globalThis;",
            );
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        assert_eq!(
            crate::test_support::eval(
                scope,
                "globalThis.__carried.n + ',' + (globalThis.__self === globalThis)",
            )
            .to_rust_string_lossy(scope),
            "7,true"
        );
    }

    /// A realm whose global the host built from a handler-bearing template — the
    /// node:vm context the CLI's snapshot host adds as slot 0 — writes a blob and
    /// reads it back instead of ending the write. What travels is the global's
    /// *state*: the handler callbacks are pointers into the process that wrote the
    /// blob, so the restored global is ordinary and does not intercept. That is
    /// the tier the module docs state, and both halves are pinned here — the blob
    /// is produced at all, and what the global carried comes back.
    #[test]
    fn a_host_objects_state_is_carried() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let template = crate::ObjectTemplate::new(scope);
            template.set_named_property_handler(crate::NamedPropertyHandlerConfiguration::new());
            let context = Context::new(
                scope,
                ContextOptions {
                    global_template: Some(template),
                    ..Default::default()
                },
            );
            let scope = &mut crate::ContextScope::new(scope, context);
            crate::test_support::eval(scope, "globalThis.__carried = 7");
            assert_eq!(scope.add_context(context), 0, "the first added context");
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, 0).expect("the blob names slot 0");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        assert_eq!(
            crate::test_support::eval_number(scope, "globalThis.__carried"),
            7.0
        );
    }

    /// A module a host attached is an item a blob carries rather than one it
    /// drops: it comes back as a module record at the index it was attached
    /// under, and the item attached after it keeps its own index.
    ///
    /// This is the shape `deno_core` builds — it attaches the module records of
    /// its ES module snapshot beside the values — and a dropped one would hand
    /// every later item back under an earlier index than the host stored it
    /// under.
    #[test]
    fn an_attached_module_round_trips_at_its_own_index() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let realm = crate::realm_of(scope);
            let module =
                api::Module::compile(&realm, "deno:core", "export const x = 1;").expect("a module");
            scope.add_context_data(context, Local::<Module>::from_module(module));
            let after: Local<Value> = Number::new(scope, 9.0).into();
            scope.add_context_data(context, after);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let module = scope
            .get_context_data_from_snapshot_once::<Module>(0)
            .expect("the attached module");
        assert!(
            module.is_source_text_module(),
            "the record came back a module, rebuilt from the source it was compiled from"
        );
        let after = scope
            .get_context_data_from_snapshot_once::<Value>(1)
            .expect("the item attached after it");
        assert_eq!(
            Local::<Number>::try_from(after)
                .ok()
                .map(|number| number.value()),
            Some(9.0),
            "the item after the module keeps its own index"
        );
    }

    /// Attaching something a blob has no item for ends the attach, naming the
    /// kind: a context is a `Data` the crate allows, and a build that stored one
    /// would write a slot holding fewer items than the host put in it, moving
    /// every later index.
    #[test]
    #[should_panic(expected = "is a context; a snapshot carries values and module records")]
    fn a_context_attached_as_context_data_is_refused() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        crate::scope!(let scope, &mut isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut crate::ContextScope::new(scope, context);
        scope.set_default_context(context);
        let other = Context::new(scope, Default::default());
        scope.add_context_data(context, other);
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
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
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
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
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
        assert!(restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT).is_none());
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
        assert!(restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT).is_none());
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
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
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
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let bound = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the bound function");
        crate::test_support::bind(scope, "restored", bound);
        assert_eq!(crate::test_support::eval_number(scope, "restored(2)"), 3.0);
    }

    /// A class constructor comes back a constructor, which is the value that
    /// stopped `deno_core`'s own snapshot: its bootstrapped realm attaches a
    /// class as context data and constructs it after the restore.
    #[test]
    fn a_class_constructor_round_trips_and_is_constructable_from_a_script() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let class = crate::test_support::eval(scope, "class C { x = 7; } C");
            scope.add_context_data(context, class);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let class = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the class constructor");
        crate::test_support::bind(scope, "restored", class);
        assert_eq!(
            crate::test_support::eval_number(scope, "new restored().x"),
            7.0
        );
        assert_eq!(
            crate::test_support::eval_number(
                scope,
                "restored.prototype.constructor === restored ? 1 : 0"
            ),
            1.0
        );
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
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
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

    /// An arrow comes back callable too: its source is the expression it was
    /// written as, which the restore evaluates in the reading realm — so a host's
    /// attached callback written as an arrow survives a snapshot, which is how a
    /// module's own helpers are usually written.
    #[test]
    fn an_arrow_round_trips_and_is_callable_from_a_script() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let arrow = crate::test_support::eval(scope, "((a) => a + 1)");
            scope.add_context_data(context, arrow);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let arrow = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the arrow");
        crate::test_support::bind(scope, "restored", arrow);
        assert_eq!(
            crate::test_support::eval_number(scope, "restored(41)"),
            42.0
        );
    }

    /// A method comes back callable, with the `[[HomeObject]]` its `super`
    /// resolves through — the one value a method record carries beside its source,
    /// and the shape a host's attached objects are usually made of.
    #[test]
    fn a_method_round_trips_and_reaches_super_from_a_script() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let method = crate::test_support::eval(
                scope,
                "var proto = { greet() { return 41; } };\n\
                 var holder = { __proto__: proto, m() { return super.greet(); } };\n\
                 holder.m",
            );
            scope.add_context_data(context, method);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let method = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the method");
        crate::test_support::bind(scope, "restored", method);
        assert_eq!(crate::test_support::eval_number(scope, "restored()"), 41.0);
    }

    /// A method that reads a private name (here a private **method**) is carried as
    /// a member of its class, so it comes back callable on an instance of the class
    /// the restore rebuilt — the operation deno's `uncurryThis(Class.prototype
    /// .method)` performs, and the value its blob's first load stopped on.
    #[test]
    fn a_private_name_method_round_trips_and_reads_its_classs_private_field() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let holder = crate::test_support::eval(
                scope,
                "var holder = {};\n\
                 holder.ctor = class Counter { #count = 3; \
                 #bump() { this.#count = this.#count + 1; return this.#count; } \
                 bump() { return this.#bump(); } };\n\
                 holder.method = holder.ctor.prototype.bump;\n\
                 holder",
            );
            scope.add_context_data(context, holder);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let holder = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the holder");
        crate::test_support::bind(scope, "restored", holder);
        assert_eq!(
            crate::test_support::eval_number(
                scope,
                "(function () { var c = new restored.ctor(); restored.method.call(c); \
                 return restored.method.call(c); })()",
            ),
            5.0
        );
    }

    /// The continuation-preserved embedder data accessors are the bridge's own
    /// callbacks too, so a blob of one comes back a function rather than a
    /// pointer the host's table does not have — and it reads the slot of the
    /// isolate it was restored **into**, which no blob carries.
    #[test]
    fn the_bridges_continuation_data_accessors_round_trip_through_the_blob() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let extras = context.get_extras_binding_object(scope);
            let key = crate::String::new(scope, "getContinuationPreservedEmbedderData").unwrap();
            let getter = extras.get(scope, key.into()).expect("the accessor");
            scope.add_context_data(context, getter);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let getter = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the accessor");
        crate::test_support::bind(scope, "restored_getter", getter);

        // The restored isolate's own slot, not the writing isolate's: that is
        // per-isolate state, and no blob carries it.
        let value = crate::test_support::eval(scope, "'the restored isolate'");
        scope.set_continuation_preserved_embedder_data(value);
        assert_eq!(
            crate::test_support::eval(scope, "restored_getter()").to_rust_string_lossy(scope),
            "the restored isolate"
        );
    }

    /// The console this bridge installs is the one host function no host had a
    /// chance to register — the bridge makes it for every context, including the
    /// ones a snapshot is taken of — so the bridge puts its callback in the table
    /// itself. A blob of a realm's console therefore comes back a console, and one
    /// of its methods is still the silent one rather than a missing reference.
    #[test]
    fn the_bridges_console_round_trips_through_the_blob() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let log = crate::test_support::eval(scope, "console.log");
            scope.add_context_data(context, log);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let log = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the console method");
        crate::test_support::bind(scope, "restored", log);
        // The record's own properties came back with it, so it is the *same*
        // method rather than a fresh anonymous function.
        assert_eq!(
            crate::test_support::eval_number(scope, "restored.name === 'log' ? 1 : 0"),
            1.0
        );
        // And it is still silent: what V8's console method does with no delegate.
        assert_eq!(
            crate::test_support::eval_number(scope, "restored('a message') === undefined ? 1 : 0"),
            1.0
        );
    }

    /// A host callback comes back a function, called through the entry the
    /// load's external-reference table holds at the index the blob wrote, with
    /// the data it was built with. This is the value deno's bootstrap installs
    /// and the engine cannot rebuild: the address belongs to the process, so what
    /// a blob can carry is which entry of the host's own table to put the call
    /// back from — and the data, which is a value.
    #[test]
    fn a_host_callback_round_trips_through_the_external_reference_table() {
        use crate::MapFnTo;
        fn an_op(
            _scope: &mut crate::scope::PinScope<'_, '_>,
            args: crate::function::FunctionCallbackArguments,
            rv: crate::function::ReturnValue,
        ) {
            // What the host attached when it built the function. A blob that did
            // not carry it would hand this `undefined` and the answer below would
            // not be the data.
            rv.set(args.data());
        }
        let references = vec![crate::ExternalReference {
            function: an_op.map_fn_to(),
        }];
        let mut isolate =
            Isolate::snapshot_creator(Some(std::borrow::Cow::Owned(references.clone())), None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            // The way deno builds an op: a callback with `new` refused and a value
            // it reads at call time, which is the kind the record can put back in
            // full.
            let template = crate::FunctionTemplate::builder(an_op)
                .constructor_behavior(crate::ConstructorBehavior::Throw)
                .data(crate::Number::new(scope, 7.0).into())
                .build(scope);
            let function = template.get_function(scope).expect("function");
            scope.add_context_data(context, function.cast::<Value>());
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from_with(blob, references);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let function = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the host callback");
        crate::test_support::bind(scope, "restored", function);
        assert_eq!(crate::test_support::eval_number(scope, "restored()"), 7.0);
    }

    /// A snapshot written **of a load**: the second blob carries the callback the
    /// first one did.
    ///
    /// A function a load rebuilt is the load's rather than the host's template's,
    /// so unless the load is told what it made, the write side cannot say which
    /// table entry it came from and the second `create_blob` refuses it. deno's
    /// warmup test is exactly this shape.
    #[test]
    fn a_snapshot_of_a_load_carries_what_the_load_rebuilt() {
        use crate::support::MapFnTo;
        use std::borrow::Cow;

        fn an_op(
            _scope: &mut crate::scope::PinScope<'_, '_>,
            args: crate::function::FunctionCallbackArguments,
            rv: crate::function::ReturnValue,
        ) {
            rv.set(args.data());
        }
        let references = vec![crate::ExternalReference {
            function: an_op.map_fn_to(),
        }];

        // The first blob, carrying a function built from a template.
        let mut creator = Isolate::snapshot_creator(Some(Cow::Owned(references.clone())), None);
        {
            crate::scope!(let scope, &mut creator);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let template = crate::FunctionTemplate::builder(an_op)
                .constructor_behavior(crate::ConstructorBehavior::Throw)
                .data(crate::Number::new(scope, 7.0).into())
                .build(scope);
            let function = template.get_function(scope).expect("function");
            scope.add_context_data(context, function.cast::<Value>());
        }
        let blob = creator
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        // The load: a creator continuing from the blob, the way a host that
        // re-snapshots a restored runtime does. Reading the item is what makes
        // the load rebuild the callback.
        let mut warm = Isolate::snapshot_creator_from_existing_snapshot(
            blob,
            Some(Cow::Owned(references.clone())),
            None,
        );
        {
            crate::scope!(let scope, &mut warm);
            let context = Context::from_snapshot(scope, DEFAULT_CONTEXT_SLOT, Default::default())
                .expect("the default context");
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let function = scope
                .get_context_data_from_snapshot_once::<Value>(0)
                .expect("the host callback");
            crate::test_support::bind(scope, "restored", function);
            assert_eq!(crate::test_support::eval_number(scope, "restored()"), 7.0);
            // Re-attach what was restored, which is what a host that snapshots
            // again does and what makes the second blob carry it.
            scope.add_context_data(context, function);
        }
        let second = warm
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a second blob");

        // And the second blob stands on its own: a fresh isolate boots from it
        // and the callback still answers what it was built with.
        let mut isolate = isolate_from_with(second, references);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the second blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let function = scope
            .get_context_data_from_snapshot_once::<Value>(0)
            .expect("the host callback");
        crate::test_support::bind(scope, "again", function);
        assert_eq!(crate::test_support::eval_number(scope, "again()"), 7.0);
    }

    /// A host callback whose pointer the build's table does not hold still ends
    /// the build, by the table's own name: the record is an index, and an index
    /// nothing would resolve is worse than a build that says which table entry to
    /// add.
    #[test]
    #[should_panic(expected = "cannot carry a host pointer")]
    fn a_host_callback_missing_from_the_creators_table_ends_the_build() {
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
            let template = crate::FunctionTemplate::builder(an_op)
                .constructor_behavior(crate::ConstructorBehavior::Throw)
                .build(scope);
            let function = template.get_function(scope).expect("function");
            scope.add_context_data(context, function.cast::<Value>());
        }
        let _ = isolate.create_blob(FunctionCodeHandling::Keep);
    }

    /// The index convention is V8's — the *added* contexts own the index space,
    /// from 0 — and a blob names every context the creator recorded, empty ones
    /// included.
    #[test]
    fn an_added_context_gets_slot_zero() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let default = Context::new(scope, Default::default());
            let added = Context::new(scope, Default::default());
            scope.set_default_context(default);
            assert_eq!(scope.add_context(added), 0);
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
        assert!(restored_context(&mut isolate, 0).is_some());
        assert!(
            restored_context(&mut isolate, 1).is_none(),
            "a slot no context was recorded for"
        );
    }

    /// Data attached to a context the creator added comes back at that
    /// context's index, in its own realm, and a slot no context was recorded for
    /// answers `None`. This is the shape `deno_core` builds:
    /// its bootstrapped realm as the added context at 0, with everything it
    /// attached on the realm.
    #[test]
    fn an_added_context_carries_its_own_data() {
        let mut isolate = Isolate::snapshot_creator(None, None);
        {
            crate::scope!(let scope, &mut isolate);
            let default = Context::new(scope, Default::default());
            let added = Context::new(scope, Default::default());
            scope.set_default_context(default);
            assert_eq!(scope.add_context(added), 0);
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
            "the default context's own data"
        );

        let mut isolate = isolate_from(blob);
        let context = restored_context(&mut isolate, 0).expect("the blob names the added context");
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
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
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
        let _ = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT);
    }

    /// A function template attached as context data comes back a template.
    ///
    /// A template has no engine object of its own — a handle is an `External`
    /// naming the address the isolate took the `Rc<FunctionTemplate>` under — so
    /// the item travels as the record `template_record` writes and the load makes
    /// a template under *its own* address, which is what lets a handle in the
    /// loading process mean anything. The callback is named through the host's
    /// external-reference table, the same table a host callback's function is
    /// named through.
    #[test]
    fn a_function_template_round_trips_as_a_template() {
        use crate::support::MapFnTo;

        fn answer(
            _scope: &mut crate::scope::PinScope<'_, '_>,
            args: crate::function::FunctionCallbackArguments,
            rv: crate::function::ReturnValue,
        ) {
            rv.set(args.data());
        }
        let references = vec![crate::ExternalReference {
            function: answer.map_fn_to(),
        }];

        let mut isolate =
            Isolate::snapshot_creator(Some(std::borrow::Cow::Owned(references.clone())), None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let template = crate::FunctionTemplate::builder(answer)
                .length(3)
                .data(crate::Number::new(scope, 7.0).into())
                .build(scope);
            template.set_class_name(crate::String::new(scope, "Answer").expect("a name"));
            // Materialize the function *before* the template is attached: a host
            // that registers a template has usually made a function from it
            // already (deno does, for every op class it registers), and the record
            // has to be readable afterwards.
            let materialized = template.get_function(scope).expect("a function");
            crate::test_support::bind(scope, "materialized", materialized.into());
            scope.add_context_data(context, template);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from_with(blob, references);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let template = scope
            .get_context_data_from_snapshot_once::<crate::FunctionTemplate>(0)
            .expect("the template");
        let function = template.get_function(scope).expect("a function");
        crate::test_support::bind(scope, "restored", function.into());
        // The callback, the data it reads, the length and the class name: the
        // parts the record carries, each one observable from the function it
        // makes.
        assert_eq!(crate::test_support::eval_number(scope, "restored()"), 7.0);
        assert_eq!(
            crate::test_support::eval_number(scope, "restored.length"),
            3.0
        );
        assert_eq!(
            crate::test_support::eval(scope, "restored.name").to_rust_string_lossy(scope),
            "Answer"
        );
    }

    /// A template with a property of its own: the record holds no property yet, so
    /// the write names the part rather than writing a template that would come
    /// back without it.
    #[test]
    #[should_panic(expected = "cannot be carried yet: a property of its own")]
    fn a_function_template_with_a_property_ends_the_build() {
        use crate::support::MapFnTo;

        fn answer(
            _scope: &mut crate::scope::PinScope<'_, '_>,
            _args: crate::function::FunctionCallbackArguments,
            _rv: crate::function::ReturnValue,
        ) {
        }
        let references = vec![crate::ExternalReference {
            function: answer.map_fn_to(),
        }];
        let mut isolate =
            Isolate::snapshot_creator(Some(std::borrow::Cow::Owned(references)), None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let template = crate::FunctionTemplate::builder(answer).build(scope);
            let key = crate::String::new(scope, "statics").expect("a key");
            template.set(key.into(), crate::Number::new(scope, 1.0).into());
            scope.add_context_data(context, template);
        }
    }

    /// A template whose callback the host's table does not hold is refused where a
    /// host callback's function is: the record names the callback as a *pointer*,
    /// and an address the loading process cannot resolve is not carried.
    #[test]
    #[should_panic(expected = "the pointer is not in the external-reference table")]
    fn a_function_template_outside_the_table_ends_the_build() {
        fn answer(
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
            let template = crate::FunctionTemplate::builder(answer).build(scope);
            scope.add_context_data(context, template);
        }
        let _ = isolate.create_blob(FunctionCodeHandling::Keep);
    }

    /// A template's parent travels with it, and two children that inherit from one
    /// parent come back sharing **it**.
    ///
    /// The record for a template is *one* object, so both children name one record
    /// for their parent, and the load makes one template per record. What that buys
    /// is observable in JavaScript: if the parent were copied per child, two
    /// subclasses would answer two different `Object.getPrototypeOf(child.prototype)`.
    #[test]
    fn a_function_templates_parent_round_trips_and_is_shared() {
        use crate::support::MapFnTo;

        fn base(
            scope: &mut crate::scope::PinScope<'_, '_>,
            _args: crate::function::FunctionCallbackArguments,
            rv: crate::function::ReturnValue,
        ) {
            rv.set(crate::Number::new(scope, 1.0).into());
        }
        let references = vec![crate::ExternalReference {
            function: base.map_fn_to(),
        }];

        let mut isolate =
            Isolate::snapshot_creator(Some(std::borrow::Cow::Owned(references.clone())), None);
        {
            crate::scope!(let scope, &mut isolate);
            let context = Context::new(scope, Default::default());
            let scope = &mut crate::ContextScope::new(scope, context);
            scope.set_default_context(context);
            let parent = crate::FunctionTemplate::builder(base).build(scope);
            parent.set_class_name(crate::String::new(scope, "Base").expect("a name"));
            let first = crate::FunctionTemplate::builder(base).build(scope);
            first.inherit(parent);
            let second = crate::FunctionTemplate::builder(base).build(scope);
            second.inherit(parent);
            // Materialized, the way a host that registers a template does — and
            // only the children are attached, so the parent travels through their
            // records alone.
            crate::test_support::bind(
                scope,
                "materialized",
                first.get_function(scope).expect("a function").into(),
            );
            scope.add_context_data(context, first);
            scope.add_context_data(context, second);
        }
        let blob = isolate
            .create_blob(FunctionCodeHandling::Keep)
            .expect("a blob");

        let mut isolate = isolate_from_with(blob, references);
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
        crate::scope!(let scope, &mut isolate);
        let context = context.open(scope);
        let scope = &mut crate::ContextScope::new(scope, context);
        let first = scope
            .get_context_data_from_snapshot_once::<crate::FunctionTemplate>(0)
            .expect("the first child");
        let second = scope
            .get_context_data_from_snapshot_once::<crate::FunctionTemplate>(1)
            .expect("the second child");
        crate::test_support::bind(
            scope,
            "First",
            first.get_function(scope).expect("a function").into(),
        );
        crate::test_support::bind(
            scope,
            "Second",
            second.get_function(scope).expect("a function").into(),
        );
        assert!(
            crate::test_support::eval(
                scope,
                "Object.getPrototypeOf(First.prototype) === Object.getPrototypeOf(Second.prototype)",
            )
            .is_true(),
            "two children of one parent share the parent they come back with"
        );
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
        let context = restored_context(&mut isolate, DEFAULT_CONTEXT_SLOT)
            .expect("the blob names the default context");
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
