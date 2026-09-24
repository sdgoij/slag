//! Array buffers and the views over them (`v8::ArrayBuffer`,
//! `v8::ArrayBufferView`).
//!
//! The engine's host-facing entries live with the built-ins
//! (`runtime::builtins::array_buffer`), because that is where the buffer state
//! and the allocation rules are. This module is the shape over them: the
//! geometry a host asks for, and the [`BackingStore`] it takes a reference to.

use std::ffi::c_void;
use std::ptr::NonNull;

use crux::handle::Handle;
use crux::object::{JsObject, ObjectKind};
use crux::typed_array::{ElementType, SharedBuffer};
use runtime::api;
use runtime::builtins::array_buffer::BufferState;
use slag::buffers::{
    array_buffer_from_block, detach_array_buffer, shared_array_buffer_from_block,
    typed_array_from_buffer, view_out_of_bounds,
};

use crate::data::{
    ArrayBuffer, ArrayBufferView, BigInt64Array, BigUint64Array, Float16Array, Float32Array,
    Float64Array, Int8Array, Int16Array, Int32Array, SharedArrayBuffer, Uint8Array,
    Uint8ClampedArray, Uint16Array, Uint32Array, Value,
};
use crate::handle::{Local, LocalHandle};
use crate::scope::PinScope;
use crate::support::{BackingStore, Rawable, SharedRef, UniqueRef};

/// How a host releases memory it lent to a backing store
/// (`v8::BackingStoreDeleterCallback`).
///
/// The engine calls it with the same three things a host passed to
/// [`ArrayBuffer::new_backing_store_from_ptr`]: the bytes, their length, and the
/// host's own context pointer.
pub type BackingStoreDeleterCallback =
    unsafe extern "C" fn(data: *mut c_void, byte_length: usize, deleter_data: *mut c_void);

/// What the agent records for one buffer object, as the store and the queries
/// below report it.
struct BufferFacts {
    block: SharedBuffer,
    byte_length: usize,
    is_shared: bool,
    was_detached: bool,
    resizable_by_user_javascript: bool,
}

impl BufferFacts {
    fn of(state: &BufferState) -> Self {
        Self {
            block: state.shared.clone(),
            byte_length: state.byte_length,
            is_shared: state.is_shared,
            was_detached: state.detached,
            // A growable `SharedArrayBuffer` is resizable by JavaScript just as
            // a resizable `ArrayBuffer` is.
            resizable_by_user_javascript: state.resizable || state.growable,
        }
    }

    fn store(&self) -> BackingStore {
        BackingStore::from_buffer(
            self.block.clone(),
            self.byte_length,
            self.resizable_by_user_javascript,
        )
    }
}

/// The buffer record for an object, if it is a registered buffer.
fn facts_for_id(id: u64) -> Option<BufferFacts> {
    crate::realm::with_agent(|agent| {
        let cell = agent.buffer_data.get(&id)?;
        Some(BufferFacts::of(&cell.borrow()))
    })?
}

fn buffer_facts(value: &api::Local) -> Option<BufferFacts> {
    facts_for_id(value.value().as_object()?.id())
}

fn facts_of_value(value: &crux::value::Value) -> Option<BufferFacts> {
    facts_for_id(value.as_object()?.id())
}

/// What a view's slots say, with the buffer it looks into resolved.
struct ViewFacts {
    buffer_object: crux::value::Value,
    buffer: BufferFacts,
    byte_offset: usize,
    /// `None` for an auto length, which tracks the buffer.
    byte_length: Option<usize>,
    out_of_bounds: bool,
}

impl ViewFacts {
    /// The length the view reports: zero once it no longer fits the buffer, and
    /// otherwise its own, tracked live when it is auto (spec 25.2.2.1 step 12).
    fn effective_byte_length(&self) -> usize {
        if self.buffer.was_detached || self.out_of_bounds {
            return 0;
        }
        self.byte_length
            .unwrap_or_else(|| self.buffer.byte_length.saturating_sub(self.byte_offset))
    }

    fn effective_byte_offset(&self) -> usize {
        if self.buffer.was_detached || self.out_of_bounds {
            return 0;
        }
        self.byte_offset
    }
}

fn view_facts(value: &api::Local) -> Option<ViewFacts> {
    let object = value.value().as_object()?;
    let (buffer_object, byte_offset, byte_length, typed_array_out_of_bounds) = match &object.kind {
        ObjectKind::IntegerIndexed(slots) => (
            slots.buffer_object,
            slots.byte_offset,
            (!slots.auto_length).then_some(slots.byte_length),
            Some(view_out_of_bounds(slots)),
        ),
        _ => {
            let id = object.id();
            let (buffer_object, byte_offset, byte_length) = crate::realm::with_agent(|agent| {
                let cell = agent.dataview_data.get(&id)?;
                let state = cell.borrow();
                Some((state.buffer_object, state.byte_offset, state.byte_length))
            })??;
            (buffer_object, byte_offset, byte_length, None)
        }
    };
    let buffer = facts_of_value(&buffer_object)?;
    let out_of_bounds = typed_array_out_of_bounds.unwrap_or_else(|| {
        is_data_view_out_of_bounds(byte_offset, byte_length, buffer.byte_length)
    });
    Some(ViewFacts {
        buffer_object,
        buffer,
        byte_offset,
        byte_length,
        out_of_bounds,
    })
}

/// IsViewOutOfBounds (spec 25.4.1.5) for a `DataView`, whose auto length tracks
/// the buffer so that only its offset can push it out.
fn is_data_view_out_of_bounds(
    byte_offset: usize,
    byte_length: Option<usize>,
    buffer_byte_length: usize,
) -> bool {
    match byte_length {
        None => byte_offset > buffer_byte_length,
        Some(length) => byte_offset + length > buffer_byte_length,
    }
}

impl ArrayBuffer {
    /// A new zeroed buffer of `byte_length` bytes (`v8::ArrayBuffer::new`).
    ///
    /// A realm always has `%ArrayBuffer.prototype%`, so the one way this can
    /// fail is an engine change to the bootstrap; failing loudly is how that
    /// gets noticed rather than a buffer of zero length.
    pub fn new<'s>(scope: &PinScope<'s, '_, ()>, byte_length: usize) -> Local<'s, ArrayBuffer> {
        let realm = crate::realm_of(scope);
        let value = realm
            .with_agent(|agent| {
                array_buffer_from_block(agent, SharedBuffer::new(byte_length), byte_length)
            })
            .expect("bridge bug: a realm can make an ArrayBuffer");
        Local::from_engine(api::Local::from(value))
    }

    /// A buffer over a backing store (`v8::ArrayBuffer::with_backing_store`).
    ///
    /// The store's block becomes the buffer's storage, so a host writing
    /// through the store — which is how a host hands bytes to JavaScript —
    /// writes what the script reads.
    pub fn with_backing_store<'s>(
        scope: &PinScope<'s, '_, ()>,
        store: &SharedRef<BackingStore>,
    ) -> Local<'s, ArrayBuffer> {
        let realm = crate::realm_of(scope);
        let block = store.block().clone();
        let byte_length = store.byte_length();
        let value = realm
            .with_agent(|agent| array_buffer_from_block(agent, block, byte_length))
            .expect("bridge bug: a realm can make an ArrayBuffer");
        Local::from_engine(api::Local::from(value))
    }

    /// A standalone store of `byte_length` zeroed bytes
    /// (`v8::ArrayBuffer::new_backing_store`).
    pub fn new_backing_store(
        _scope: &mut crate::Isolate,
        byte_length: usize,
    ) -> UniqueRef<BackingStore> {
        UniqueRef::new(BackingStore::new(byte_length))
    }

    /// A standalone store that owns `bytes`
    /// (`v8::ArrayBuffer::new_backing_store_from_boxed_slice`).
    pub fn new_backing_store_from_boxed_slice(bytes: Box<[u8]>) -> UniqueRef<BackingStore> {
        UniqueRef::new(BackingStore::from_bytes(&bytes))
    }

    /// [`new_backing_store_from_boxed_slice`](Self::new_backing_store_from_boxed_slice)
    /// over a `Vec` (`v8::ArrayBuffer::new_backing_store_from_vec`).
    pub fn new_backing_store_from_vec(bytes: Vec<u8>) -> UniqueRef<BackingStore> {
        Self::new_backing_store_from_boxed_slice(bytes.into_boxed_slice())
    }

    /// A standalone store over `bytes`, which the caller hands over
    /// (`v8::ArrayBuffer::new_backing_store_from_bytes`).
    ///
    /// The crate we stand in for takes ownership of the bytes and lets V8 read
    /// them where they are, freeing them when the store dies. Here they are read
    /// into the engine's block, so the caller's buffer is released as this
    /// returns — the same contents at the same length, and the only thing that
    /// could tell the two apart is the address [`data`](BackingStore::data)
    /// answers with.
    pub fn new_backing_store_from_bytes<T: Rawable>(bytes: T) -> UniqueRef<BackingStore> {
        UniqueRef::new(BackingStore::from_bytes(bytes.as_bytes()))
    }

    /// A standalone store over memory the *host* owns, borrowed rather than
    /// copied (`v8::ArrayBuffer::new_backing_store_from_ptr`).
    ///
    /// This is the aliasing constructor, and the difference from
    /// [`new_backing_store_from_bytes`](Self::new_backing_store_from_bytes) is
    /// the whole point of it: the host keeps the allocation, the engine reads and
    /// writes it where it is, so a script writing through a buffer over this
    /// store writes the host's memory and a host writing there is what the script
    /// reads. `deleter_callback` is called with `deleter_data` when the last
    /// reference to the store goes, which is where the host gets its memory back
    /// — the same moment V8 calls it.
    ///
    /// A buffer over such a store is never resizable: the host's allocation is
    /// what it is.
    ///
    /// # Safety
    ///
    /// `data_ptr` must point at `byte_length` bytes that stay readable and
    /// writable until the deleter runs, must be sound to access as bytes, and
    /// must be released by `deleter_callback` and nothing else for as long as the
    /// store lives.
    pub unsafe fn new_backing_store_from_ptr(
        data_ptr: *mut c_void,
        byte_length: usize,
        deleter_callback: BackingStoreDeleterCallback,
        deleter_data: *mut c_void,
    ) -> UniqueRef<BackingStore> {
        // The closure is built outside the `unsafe` block below so that its own
        // `unsafe` call is the one it reads as: the deleter runs long after this
        // function returns, and the block it needs is the call's, not this
        // function's.
        let release = move || {
            // SAFETY: the caller promised this is the way to release the bytes,
            // and this is the only call it gets.
            unsafe { deleter_callback(data_ptr, byte_length, deleter_data) };
        };
        // SAFETY: the caller's contract — the bytes stay valid until `release`
        // runs, and `release` is the one call that runs it.
        let block = unsafe {
            SharedBuffer::borrowed(data_ptr.cast::<u8>(), byte_length, Some(Box::new(release)))
        };
        UniqueRef::new(BackingStore::from_buffer(block, byte_length, false))
    }
}

impl<'s> LocalHandle<'s, ArrayBuffer> {
    /// The byte length (`v8::ArrayBuffer::byte_length`): zero once detached.
    pub fn byte_length(&self) -> usize {
        buffer_facts(self.engine()).map_or(0, |facts| facts.byte_length)
    }

    /// Whether the buffer may be detached (`v8::ArrayBuffer::is_detachable`).
    ///
    /// A `SharedArrayBuffer` may not, which is the engine's rule as well. V8
    /// refuses for one wasm owns too; the engine's byte block does not record
    /// that, and this build has no wasm to own one.
    pub fn is_detachable(&self) -> bool {
        buffer_facts(self.engine()).is_some_and(|facts| !facts.is_shared)
    }

    /// Whether the buffer was detached (`v8::ArrayBuffer::was_detached`).
    pub fn was_detached(&self) -> bool {
        buffer_facts(self.engine()).is_some_and(|facts| facts.was_detached)
    }

    /// Detach the buffer and every view over it (`v8::ArrayBuffer::detach`).
    ///
    /// The engine records no `[[ArrayBufferDetachKey]]`, so a key has nothing
    /// to match against and nothing to protect: no key, or `undefined`, detaches,
    /// and any other key is refused the way a mismatch is — with `None` and no
    /// detach. A buffer that cannot be detached at all answers as though it were
    /// detached, which is what the crate we stand in for does to keep V8 from
    /// terminating on one.
    pub fn detach(&self, key: Option<Local<'_, Value>>) -> Option<bool> {
        if !self.is_detachable() {
            return Some(true);
        }
        if key.is_some_and(|key| !key.is_undefined()) {
            return None;
        }
        let id = self.engine().value().as_object()?.id();
        crate::realm::with_agent(|agent| detach_array_buffer(agent, id))?;
        Some(true)
    }

    /// A shared reference to the bytes (`v8::ArrayBuffer::get_backing_store`).
    ///
    /// A detached buffer hands out a store of zero length, as there: the bytes
    /// are gone from JavaScript's point of view even though the block is still
    /// allocated.
    pub fn get_backing_store(&self) -> SharedRef<BackingStore> {
        let facts = buffer_facts(self.engine())
            .expect("bridge bug: an ArrayBuffer handle needs the realm it came from");
        SharedRef::new(facts.store())
    }

    /// The first byte of the storage (`v8::ArrayBuffer::data`).
    pub fn data(&self) -> Option<NonNull<c_void>> {
        self.get_backing_store().data()
    }
}

impl SharedArrayBuffer {
    /// A shared buffer over a backing store
    /// (`v8::SharedArrayBuffer::with_backing_store`).
    ///
    /// The store's block becomes the buffer's storage, as it does for
    /// [`ArrayBuffer::with_backing_store`](ArrayBuffer::with_backing_store): a
    /// host writing through the store writes what a reader of the buffer sees,
    /// and — unlike an `ArrayBuffer` over the same store — an agent other than
    /// this one can read it.
    ///
    /// The *block* is marked shared here as well as the buffer record. The
    /// engine's language-facing constructor does both; its host-facing one
    /// records only the buffer, so a host that asks the store afterwards would be
    /// told it is unshared without this line.
    pub fn with_backing_store<'s>(
        scope: &PinScope<'s, '_, ()>,
        store: &SharedRef<BackingStore>,
    ) -> Local<'s, SharedArrayBuffer> {
        store.block().mark_shared();
        let realm = crate::realm_of(scope);
        let block = store.block().clone();
        let byte_length = store.byte_length();
        let value = realm
            .with_agent(|agent| shared_array_buffer_from_block(agent, block, byte_length))
            .expect("bridge bug: a realm can make a SharedArrayBuffer");
        Local::from_engine(api::Local::from(value))
    }
}

impl<'s> LocalHandle<'s, SharedArrayBuffer> {
    /// A shared reference to the bytes
    /// (`v8::SharedArrayBuffer::get_backing_store`).
    ///
    /// The same record an `ArrayBuffer` reads, because the engine keeps one
    /// table for both kinds of buffer; the store's own `is_shared` is what tells
    /// them apart.
    pub fn get_backing_store(&self) -> SharedRef<BackingStore> {
        let facts = buffer_facts(self.engine())
            .expect("bridge bug: a SharedArrayBuffer handle needs the realm it came from");
        SharedRef::new(facts.store())
    }
}

impl<'s> LocalHandle<'s, ArrayBufferView> {
    /// The buffer the view looks into (`v8::ArrayBufferView::buffer`).
    pub fn buffer<'a>(&self, _scope: &PinScope<'a, '_>) -> Option<Local<'a, ArrayBuffer>> {
        let object = view_facts(self.engine())?.buffer_object.as_object()?;
        Some(Local::from_engine(api::Local::from(
            crux::value::Value::Object(object),
        )))
    }

    /// Whether the view has a buffer allocated
    /// (`v8::ArrayBufferView::has_buffer`).
    pub fn has_buffer(&self) -> bool {
        view_facts(self.engine()).is_some_and(|facts| facts.buffer_object.as_object().is_some())
    }

    /// A shared reference to the viewed buffer's bytes
    /// (`v8::ArrayBufferView::get_backing_store`).
    pub fn get_backing_store(&self) -> Option<SharedRef<BackingStore>> {
        let facts = view_facts(self.engine())?;
        Some(SharedRef::new(facts.buffer.store()))
    }

    /// The first byte of the view's range (`v8::ArrayBufferView::data`).
    ///
    /// V8's answer points into the buffer's storage whatever the view's state;
    /// the engine keeps the block allocated across detaching, so this one stays
    /// dereferenceable where V8's would not.
    pub fn data(&self) -> *mut c_void {
        let Some(facts) = view_facts(self.engine()) else {
            return std::ptr::null_mut();
        };
        facts
            .buffer
            .block
            .data_ptr()
            .wrapping_add(facts.byte_offset)
            .cast::<c_void>()
    }

    /// The view's byte length (`v8::ArrayBufferView::byte_length`): zero once
    /// the view no longer fits its buffer.
    pub fn byte_length(&self) -> usize {
        view_facts(self.engine()).map_or(0, |facts| facts.effective_byte_length())
    }

    /// The view's byte offset (`v8::ArrayBufferView::byte_offset`): zero once
    /// the view no longer fits its buffer.
    pub fn byte_offset(&self) -> usize {
        view_facts(self.engine()).map_or(0, |facts| facts.effective_byte_offset())
    }

    /// The view's contents as a pointer and a length
    /// (`v8::ArrayBufferView::get_contents_raw_parts`).
    ///
    /// The crate we stand in for copies contents that live inside the object
    /// into `storage` and points into it; the engine never stores view elements
    /// in the object, so `storage` is not written and the answer is the view's
    /// own range. [`crate::TYPED_ARRAY_MAX_SIZE_IN_HEAP`] is zero, so the
    /// storage a host sizes for that case is empty either way.
    ///
    /// # Safety
    ///
    /// The pointer is the live buffer's, so the caller must not resize or
    /// detach the buffer while holding it, and a JavaScript callback between
    /// the call and the use can do exactly that.
    pub unsafe fn get_contents_raw_parts(&self, storage: &mut [u8]) -> (*mut u8, usize) {
        let _ = storage;
        (self.data().cast::<u8>(), self.byte_length())
    }

    /// The view's contents as a slice, borrowed from `storage` when the crate we
    /// stand in for copied them there
    /// (`v8::ArrayBufferView::get_contents`).
    pub fn get_contents<'a>(&'a self, storage: &'a mut [u8]) -> &'a [u8] {
        // SAFETY: the returned slice borrows `self` for as long as the slice's
        // lifetime, which is the borrow a host has to respect to keep the
        // buffer alive across it.
        let (data, length) = unsafe { self.get_contents_raw_parts(storage) };
        if data.is_null() {
            return &[];
        }
        // SAFETY: as above — the view's range is `length` readable bytes, and
        // the borrow keeps them valid for the slice's life.
        unsafe { std::slice::from_raw_parts(data, length) }
    }

    /// Copy the view's contents into `dest`, answering the bytes written
    /// (`v8::ArrayBufferView::copy_contents`).
    pub fn copy_contents(&self, dest: &mut [u8]) -> usize {
        let length = self.byte_length().min(dest.len());
        if length == 0 {
            return 0;
        }
        // SAFETY: the range copied is the view's own, clipped to the caller's
        // buffer; copying does not outlive the call.
        let source = unsafe { std::slice::from_raw_parts(self.data().cast::<u8>(), length) };
        dest[..length].copy_from_slice(source);
        length
    }

    /// Copy the view's contents into uninitialized memory
    /// (`v8::ArrayBufferView::copy_contents_uninit`), answering the bytes
    /// written.
    pub fn copy_contents_uninit(&self, dest: &mut [std::mem::MaybeUninit<u8>]) -> usize {
        let length = self.byte_length().min(dest.len());
        if length == 0 {
            return 0;
        }
        // SAFETY: the range copied is the view's own, clipped to the caller's
        // buffer; writing `u8`s over uninitialized bytes is what the type
        // admits.
        let source = unsafe { std::slice::from_raw_parts(self.data().cast::<u8>(), length) };
        let dest =
            unsafe { std::slice::from_raw_parts_mut(dest.as_mut_ptr().cast::<u8>(), length) };
        dest.copy_from_slice(source);
        length
    }
}

/// The `%X.prototype%` intrinsic views of an element type are created with,
/// which is the prototype the engine's constructor path takes.
fn typed_array_prototype(
    scope: &PinScope<'_, '_>,
    element_type: ElementType,
) -> Option<Handle<JsObject>> {
    let name = format!("%{}Array.prototype%", element_type.name());
    crate::realm_of(scope).intrinsic(&name)?.as_object()
}

/// A view over `buffer`, built the way the engine's constructor builds one.
///
/// `None` means the range does not fit the buffer and a pending exception says
/// so, which is what the crate we stand in for reports the same way.
fn typed_array_new<'s, T>(
    scope: &PinScope<'s, '_>,
    element_type: ElementType,
    buffer: Local<'_, ArrayBuffer>,
    byte_offset: usize,
    length: usize,
) -> Option<Local<'s, T>> {
    let prototype = typed_array_prototype(scope, element_type)?;
    let buffer = *buffer.engine().value();
    let args = [
        crux::value::Value::Number(byte_offset as f64),
        crux::value::Value::Number(length as f64),
    ];
    let realm = crate::realm_of(scope);
    let built = realm.with_agent(|agent| {
        typed_array_from_buffer(agent, prototype, element_type, &buffer, &args)
    });
    match built {
        Ok(value) => Some(Local::from_engine(api::Local::from(value))),
        Err(error) => {
            crate::throw(scope, &error);
            None
        }
    }
}

/// One view constructor per element type, as the crate we stand in for
/// declares them: `v8::Uint8Array::new(scope, buffer, byte_offset, length)`,
/// with the length in elements.
macro_rules! typed_array_constructors {
    ($($tag:ident => $element:ident),* $(,)?) => {
        $(
            impl $tag {
                #[doc = concat!(
                    "A view over `buffer` starting `byte_offset` bytes in and covering `length` elements (`v8::",
                    stringify!($tag),
                    "::new`), or `None` when that range does not fit the buffer."
                )]
                pub fn new<'s>(
                    scope: &PinScope<'s, '_>,
                    buffer: Local<'_, ArrayBuffer>,
                    byte_offset: usize,
                    length: usize,
                ) -> Option<Local<'s, $tag>> {
                    typed_array_new(scope, ElementType::$element, buffer, byte_offset, length)
                }
            }
        )*
    };
}

typed_array_constructors! {
    Uint8Array => Uint8,
    Uint8ClampedArray => Uint8Clamped,
    Int8Array => Int8,
    Uint16Array => Uint16,
    Int16Array => Int16,
    Uint32Array => Uint32,
    Int32Array => Int32,
    Float16Array => Float16,
    Float32Array => Float32,
    Float64Array => Float64,
    BigInt64Array => BigInt64,
    BigUint64Array => BigUint64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{bind, eval, eval_number, in_context};

    /// Bytes a host owns become the buffer's storage, at their width: a
    /// `Box<[u16]>` is its elements in bytes, not its count.
    #[test]
    fn a_store_from_bytes_carries_them_at_their_width() {
        in_context!(scope, {
            let store = ArrayBuffer::new_backing_store_from_bytes(vec![1u8, 2, 3]).make_shared();
            assert_eq!(store.byte_length(), 3);
            let buffer = ArrayBuffer::with_backing_store(scope, &store);
            bind(scope, "ab", buffer.cast::<Value>());
            assert_eq!(eval_number(scope, "new Uint8Array(ab)[2]"), 3.0);

            let wide: Box<[u16]> = vec![0x0102u16, 0x0304].into_boxed_slice();
            let store = ArrayBuffer::new_backing_store_from_bytes(wide).make_shared();
            assert_eq!(store.byte_length(), 4, "two `u16`s are four bytes");
            let buffer = ArrayBuffer::with_backing_store(scope, &store);
            bind(scope, "wide", buffer.cast::<Value>());
            assert_eq!(
                eval_number(scope, "new Uint16Array(wide)[1]"),
                0x0304 as f64,
                "the elements read back as themselves, not as their bytes"
            );
        });
    }

    /// A shared buffer over a store reads the store's bytes, and the store says
    /// it is shared afterwards — which is the line the bridge adds, because the
    /// engine's host-facing constructor records only the buffer — and the buffer's
    /// own block is marked by the engine, because it is a separate block now.
    #[test]
    fn a_shared_buffer_over_a_store_shares_its_bytes() {
        in_context!(scope, {
            let store = ArrayBuffer::new_backing_store_from_bytes(vec![7u8, 8]).make_shared();
            assert!(!store.is_shared(), "a store from bytes starts unshared");

            let shared = SharedArrayBuffer::with_backing_store(scope, &store);
            bind(scope, "sab", shared.cast::<Value>());
            assert_eq!(eval_number(scope, "new Uint8Array(sab)[0]"), 7.0);
            assert_eq!(eval_number(scope, "new Uint8Array(sab).length"), 2.0);
            assert!(
                store.is_shared(),
                "the block is marked shared with the buffer"
            );
            assert!(
                shared.get_backing_store().is_shared(),
                "and the buffer's own block is too: its flags are the buffer's,\n                 not the store's (`.notes/embedding.md` §9)"
            );

            // A write through the buffer is a write into the store, which is what
            // makes this the host's way of handing bytes to JavaScript.
            eval(scope, "Atomics.store(new Uint8Array(sab), 1, 9)");
            assert_eq!(byte_of(&store, 1), 9);
        });
    }

    /// Host memory lent to a store is the buffer's storage in both directions: a
    /// script's write lands in the host's allocation, and the host's write is what
    /// a script reads. The deleter runs when the last holder goes — not while the
    /// buffer still has it, and exactly once.
    #[test]
    fn a_buffer_over_host_memory_aliases_it() {
        static FREED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        unsafe extern "C" fn release(
            _data: *mut std::ffi::c_void,
            _byte_length: usize,
            _deleter_data: *mut std::ffi::c_void,
        ) {
            FREED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        let mut host = vec![11u8, 22, 33, 44];
        in_context!(scope, {
            // SAFETY: `host` outlives the store, and `release` is the only thing
            // that touches its allocation afterwards.
            let store = unsafe {
                ArrayBuffer::new_backing_store_from_ptr(
                    host.as_mut_ptr().cast::<std::ffi::c_void>(),
                    host.len(),
                    release,
                    std::ptr::null_mut(),
                )
            }
            .make_shared();
            let buffer = ArrayBuffer::with_backing_store(scope, &store);
            bind(scope, "ab", buffer.cast::<Value>());
            assert_eq!(eval_number(scope, "new Uint8Array(ab)[1]"), 22.0);

            eval(scope, "new Uint8Array(ab)[1] = 99");
            assert_eq!(host[1], 99, "the script wrote the host's memory");

            host[0] = 77;
            assert_eq!(
                eval_number(scope, "new Uint8Array(ab)[0]"),
                77.0,
                "the host's write is what the script reads"
            );
            assert_eq!(buffer.byte_length(), 4);

            // Dropping the store is not the end of the memory: the buffer holds
            // the block, so the host is not told its bytes are free yet. *When*
            // they are free is the isolate's lifetime, which
            // `a_buffer_over_host_memory_releases_it_when_the_isolate_goes`
            // measures — this macro's isolate outlives its body, so this test
            // cannot see the release itself.
            drop(store);
            assert_eq!(FREED.load(std::sync::atomic::Ordering::SeqCst), 0);
        });
    }

    /// When the host's memory comes back: the store holds the block, the buffer
    /// holds the block, and the isolate holds the buffer — so the release is the
    /// isolate going, and this measures exactly that.
    #[test]
    fn a_buffer_over_host_memory_releases_it_when_the_isolate_goes() {
        static RELEASED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        unsafe extern "C" fn release(
            _data: *mut std::ffi::c_void,
            _byte_length: usize,
            _deleter_data: *mut std::ffi::c_void,
        ) {
            RELEASED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        let mut host = vec![1u8, 2, 3, 4];
        {
            let mut isolate = crate::Isolate::new(crate::CreateParams::default());
            crate::scope!(let handle_scope, &mut isolate);
            let context = crate::Context::new(handle_scope, Default::default());
            let scope = &mut crate::ContextScope::new(handle_scope, context);

            // SAFETY: `host` outlives the block, and `release` is the only thing
            // that touches its allocation afterwards.
            let store = unsafe {
                ArrayBuffer::new_backing_store_from_ptr(
                    host.as_mut_ptr().cast::<std::ffi::c_void>(),
                    host.len(),
                    release,
                    std::ptr::null_mut(),
                )
            }
            .make_shared();
            let buffer = ArrayBuffer::with_backing_store(scope, &store);
            bind(scope, "ab", buffer.cast::<Value>());
            assert_eq!(eval_number(scope, "new Uint8Array(ab)[0]"), 1.0);

            drop(store);
            assert_eq!(
                RELEASED.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "the buffer still holds the host's memory"
            );
        }
        assert_eq!(
            RELEASED.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "and the host gets it back when the isolate goes"
        );
    }

    fn byte_of(store: &BackingStore, index: usize) -> u8 {
        let data = store.data().expect("a non-empty store has data");
        unsafe { *data.as_ptr().cast::<u8>().add(index) }
    }

    fn write_byte(store: &BackingStore, index: usize, byte: u8) {
        let data = store.data().expect("a non-empty store has data");
        unsafe { *data.as_ptr().cast::<u8>().add(index) = byte };
    }

    #[test]
    fn a_new_buffer_is_zeroed_and_detachable() {
        in_context!(scope, {
            let buffer = ArrayBuffer::new(scope, 8);
            assert_eq!(buffer.byte_length(), 8);
            assert!(buffer.is_detachable());
            assert!(!buffer.was_detached());

            let store = buffer.get_backing_store();
            assert_eq!(store.byte_length(), 8);
            assert_eq!(store.len(), 8);
            assert!(!store.is_shared());
            assert!(!store.is_resizable_by_user_javascript());
            assert_eq!(byte_of(&store, 0), 0);
            assert_eq!(byte_of(&store, 7), 0);
        });
    }

    /// A store's bytes are the bytes the script reads, which is the whole point
    /// of handing a host a pointer into them.
    #[test]
    fn writing_through_a_store_writes_what_the_script_reads() {
        in_context!(scope, {
            let buffer =
                Local::<ArrayBuffer>::try_from(eval(scope, "globalThis.b = new ArrayBuffer(8)"))
                    .expect("array buffer");
            write_byte(&buffer.get_backing_store(), 0, 7);
            assert_eq!(eval_number(scope, "new DataView(b).getUint8(0)"), 7.0);

            // And the other way: what the script writes, the store shows.
            eval(scope, "new DataView(b).setUint8(3, 9)");
            assert_eq!(byte_of(&buffer.get_backing_store(), 3), 9);
        });
    }

    #[test]
    fn a_buffer_can_be_built_over_a_host_allocated_store() {
        in_context!(scope, {
            let store = ArrayBuffer::new_backing_store(scope, 4);
            assert_eq!(store.byte_length(), 4);
            write_byte(&store, 3, 6);

            let buffer = ArrayBuffer::with_backing_store(scope, &store.make_shared());
            bind(scope, "b", buffer.into());
            assert_eq!(eval_number(scope, "new DataView(b).getUint8(3)"), 6.0);

            // A second buffer over the same store is the same bytes.
            let twin = ArrayBuffer::with_backing_store(scope, &buffer.get_backing_store());
            bind(scope, "t", twin.into());
            eval(scope, "new DataView(t).setUint8(0, 1)");
            assert_eq!(byte_of(&buffer.get_backing_store(), 0), 1);
        });
    }

    #[test]
    fn a_store_can_be_built_from_bytes() {
        let store =
            ArrayBuffer::new_backing_store_from_boxed_slice(vec![1, 2, 3].into_boxed_slice());
        assert_eq!(store.len(), 3);
        assert_eq!(byte_of(&store, 0), 1);
        assert_eq!(byte_of(&store, 2), 3);

        let store = ArrayBuffer::new_backing_store_from_vec(vec![4, 5]);
        assert_eq!(store.byte_length(), 2);
        assert_eq!(byte_of(&store, 1), 5);
    }

    /// A shared buffer is not detachable, and the cast to `ArrayBuffer` is not
    /// the cast to make for one.
    #[test]
    fn a_shared_buffer_is_told_apart() {
        in_context!(scope, {
            let value = eval(scope, "new SharedArrayBuffer(8)");
            assert!(value.is_shared_array_buffer());
            assert!(
                Local::<ArrayBuffer>::try_from(value).is_err(),
                "a SharedArrayBuffer is not an ArrayBuffer"
            );
        });
    }

    /// A view's geometry comes from its own slots: a typed array's or a
    /// `DataView`'s, which are in different places.
    #[test]
    fn a_view_reports_its_geometry_and_its_buffer() {
        in_context!(scope, {
            let view =
                Local::<ArrayBufferView>::try_from(eval(scope, "new Uint8Array(4)")).expect("view");
            assert_eq!(view.byte_length(), 4);
            assert_eq!(view.byte_offset(), 0);
            assert!(view.has_buffer());
            assert!(!view.data().is_null());
            assert_eq!(view.buffer(scope).expect("buffer").byte_length(), 4);
            let store = view.get_backing_store().expect("a view has a store");
            assert_eq!(store.byte_length(), 4);
            assert_eq!(store.len(), 4);

            let data_view = Local::<ArrayBufferView>::try_from(eval(
                scope,
                "new DataView(new ArrayBuffer(8), 2, 4)",
            ))
            .expect("view");
            assert_eq!(data_view.byte_offset(), 2);
            assert_eq!(data_view.byte_length(), 4);
            assert_eq!(data_view.buffer(scope).expect("buffer").byte_length(), 8);
        });
    }

    /// Writing through a view's pointer is writing the element the script sees.
    #[test]
    fn a_views_data_is_the_element_the_script_sees() {
        in_context!(scope, {
            let view =
                Local::<ArrayBufferView>::try_from(eval(scope, "globalThis.v = new Uint8Array(2)"))
                    .expect("view");
            let data = view.data();
            unsafe { *data.cast::<u8>().add(1) = 5 };
            assert_eq!(eval_number(scope, "v[1]"), 5.0);
        });
    }

    /// Detaching is observable through a view that outlives it: no range, and a
    /// store of zero length, which is what the crate we stand in for reports.
    #[test]
    fn a_detached_buffer_leaves_its_view_with_nothing() {
        in_context!(scope, {
            let view = Local::<ArrayBufferView>::try_from(eval(
                scope,
                "globalThis.b = new ArrayBuffer(8); globalThis.v = new Uint8Array(b)",
            ))
            .expect("view");
            assert_eq!(view.byte_length(), 8);

            eval(scope, "b.transfer()");

            assert_eq!(view.byte_length(), 0);
            assert_eq!(view.byte_offset(), 0);
            assert_eq!(view.get_backing_store().expect("store").byte_length(), 0);
        });
    }

    #[test]
    fn a_host_can_detach_a_buffer() {
        in_context!(scope, {
            let view = Local::<ArrayBufferView>::try_from(eval(
                scope,
                "globalThis.b = new ArrayBuffer(8); globalThis.v = new Uint8Array(b)",
            ))
            .expect("view");
            let buffer = Local::<ArrayBuffer>::try_from(eval(scope, "b")).expect("array buffer");

            assert_eq!(buffer.detach(None), Some(true));
            assert!(buffer.was_detached());
            assert_eq!(buffer.byte_length(), 0);
            assert_eq!(eval_number(scope, "b.byteLength"), 0.0);
            assert_eq!(eval_number(scope, "v.byteLength"), 0.0);
            assert_eq!(view.byte_length(), 0);
            assert_eq!(view.get_backing_store().expect("store").byte_length(), 0);
        });
    }

    /// The engine records no `[[ArrayBufferDetachKey]]`, so a key cannot match
    /// and a detach that names one is refused without detaching — the same
    /// answer a mismatch gives there.
    #[test]
    fn a_keyed_detach_is_refused() {
        in_context!(scope, {
            let buffer =
                Local::<ArrayBuffer>::try_from(eval(scope, "globalThis.b = new ArrayBuffer(8)"))
                    .expect("array buffer");

            let key = eval(scope, "'a key'");
            assert_eq!(buffer.detach(Some(key)), None);
            assert!(!buffer.was_detached());
            assert_eq!(buffer.byte_length(), 8);

            // `undefined` is the key that is stored when none was set.
            let undefined = eval(scope, "undefined");
            assert_eq!(buffer.detach(Some(undefined)), Some(true));
            assert!(buffer.was_detached());
        });
    }

    /// A host builds a view over a buffer that came from a store it filled —
    /// deno's `slice_to_uint8array` step for step: a fresh store, the bytes
    /// copied in through it, `make_shared`, a buffer over the store, and a view
    /// over that buffer.
    #[test]
    fn a_view_over_a_store_the_host_filled_reads_its_bytes() {
        in_context!(scope, {
            let bytes = [5u8, 10, 15];
            let store = ArrayBuffer::new_backing_store(scope, bytes.len());
            write_byte(&store, 0, bytes[0]);
            write_byte(&store, 1, bytes[1]);
            write_byte(&store, 2, bytes[2]);

            let buffer = ArrayBuffer::with_backing_store(scope, &store.make_shared());
            bind(scope, "ab", buffer.cast::<Value>());
            assert_eq!(
                eval_number(scope, "new Uint8Array(ab)[0]"),
                5.0,
                "the constructor path reads the store"
            );

            let view = Uint8Array::new(scope, buffer, 0, bytes.len()).expect("view");
            assert_eq!(view.byte_length(), 3);
            bind(scope, "v", view.into());
            assert_eq!(eval_number(scope, "v.byteLength"), 3.0);
            assert_eq!(
                eval_number(scope, "v[0]"),
                5.0,
                "the host-built view reads it too"
            );
            assert_eq!(eval_number(scope, "v[1]"), 10.0);
        });
    }

    /// A store taken from a buffer keeps its bytes after the buffer is
    /// detached, and a buffer built over that store is a live buffer — which is
    /// deno's `to_v8_slice_detachable` hand-over: read the store, detach, hand
    /// the store's bytes to a new view.
    #[test]
    fn a_store_outlives_the_buffer_it_came_from() {
        in_context!(scope, {
            Local::<Uint8Array>::try_from(eval(
                scope,
                "globalThis.a = new Uint8Array([5,10,15]); a.subarray(0,3)",
            ))
            .expect("view");
            let buffer = Local::<ArrayBuffer>::try_from(eval(scope, "a.buffer")).expect("buffer");
            let store = buffer.get_backing_store();
            assert_eq!(byte_of(&store, 0), 5);

            assert_eq!(buffer.detach(None), Some(true));
            assert_eq!(byte_of(&store, 0), 5, "the store outlives the detach");

            let rebuilt = ArrayBuffer::with_backing_store(scope, &store);
            let view = Uint8Array::new(scope, rebuilt, 0, 3).expect("view");
            bind(scope, "v", view.into());
            assert_eq!(eval_number(scope, "v.byteLength"), 3.0);
            assert_eq!(
                eval_number(scope, "v[0]"),
                5.0,
                "a buffer over the store is not the detached one"
            );
            assert_eq!(eval_number(scope, "v[1]"), 10.0);
        });
    }

    /// A host builds a view over its own buffer the way the constructor does,
    /// and the pointer it takes is the element the script reads.
    #[test]
    fn a_view_can_be_built_over_a_buffer_the_host_holds() {
        in_context!(scope, {
            let buffer = ArrayBuffer::new(scope, 8);
            let view = Uint8Array::new(scope, buffer, 2, 4).expect("view");
            assert_eq!(view.byte_length(), 4);
            assert_eq!(view.byte_offset(), 2);
            assert_eq!(view.buffer(scope).expect("buffer"), buffer);

            bind(scope, "v", view.into());
            assert_eq!(eval_number(scope, "v.byteOffset"), 2.0);
            assert_eq!(eval_number(scope, "v.length"), 4.0);

            let store = view.get_backing_store().expect("a view has a store");
            write_byte(&store, 2, 3);
            assert_eq!(eval_number(scope, "v[0]"), 3.0);
        });
    }

    /// The range rules are the constructor's, not a second copy of them.
    #[test]
    fn a_view_that_does_not_fit_is_refused() {
        in_context!(scope, {
            let buffer = ArrayBuffer::new(scope, 8);
            assert!(Uint8Array::new(scope, buffer, 6, 4).is_none());
            assert!(Uint8Array::new(scope, buffer, 0, 8).is_some());

            // A byte offset has to be a whole number of elements.
            let words = ArrayBuffer::new(scope, 8);
            assert!(Uint32Array::new(scope, words, 2, 1).is_none());
            assert!(Uint32Array::new(scope, words, 4, 1).is_some());

            let floats = ArrayBuffer::new(scope, 8);
            assert!(Float64Array::new(scope, floats, 0, 1).is_some());
        });
    }
}
