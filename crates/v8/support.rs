//! References to resources the engine holds ([`UniqueRef`], [`SharedRef`]) and
//! the [`BackingStore`] they carry.
//!
//! In the crate we stand in for these are pointers into a C++ allocation that
//! V8 refcounts; here the bytes live in the engine's byte block, which is
//! refcounted on its own and carries the flags the buffer set, so a reference
//! is the Rust container that keeps that block alive.

use std::ffi::c_void;
use std::fmt;
use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::Arc;

use crux::typed_array::SharedBuffer;

/// A buffer a host hands over as bytes
/// (the crate we stand in for's sealed `Rawable`).
///
/// There, the bytes are taken by value and V8 reads them where they are; here
/// they are read into an engine block, so all this asks a type for is a *view* of
/// them. What a host can observe — how many bytes and what is in them — is the
/// same either way.
pub trait Rawable {
    /// The length in bytes (`byte_len` there), which for an element slice is its
    /// element count times the element's size.
    fn byte_len(&self) -> usize;

    /// The bytes.
    fn as_bytes(&self) -> &[u8];
}

macro_rules! rawable {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Rawable for Box<[$ty]> {
                fn byte_len(&self) -> usize {
                    self.len() * size_of::<$ty>()
                }

                fn as_bytes(&self) -> &[u8] {
                    // SAFETY: the element type is a primitive scalar — no padding,
                    // every bit pattern a value, alignment no wider than the type
                    // — so its elements in memory are exactly their bytes.
                    unsafe {
                        std::slice::from_raw_parts(self.as_ptr().cast::<u8>(), self.byte_len())
                    }
                }
            }

            impl Rawable for Vec<$ty> {
                fn byte_len(&self) -> usize {
                    self.len() * size_of::<$ty>()
                }

                fn as_bytes(&self) -> &[u8] {
                    // SAFETY: as the `Box<[$ty]>` impl above.
                    unsafe {
                        std::slice::from_raw_parts(self.as_ptr().cast::<u8>(), self.byte_len())
                    }
                }
            }
        )*
    };
}

rawable!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

/// A uniquely owned resource (`v8::UniqueRef`).
pub struct UniqueRef<T>(T);

impl<T> UniqueRef<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(value)
    }

    /// Share this reference (`v8::UniqueRef::make_shared`).
    pub fn make_shared(self) -> SharedRef<T> {
        SharedRef(Arc::new(self.0))
    }

    /// The owned value, for a caller taking it apart where the crate we stand
    /// in for would release a raw pointer.
    pub(crate) fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Deref for UniqueRef<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for UniqueRef<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> fmt::Debug for UniqueRef<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UniqueRef(..)")
    }
}

/// A shared reference to a resource (`v8::SharedRef`).
///
/// `Arc`, not `Rc`, because the crate we stand in for's `SharedRef` is
/// thread-safe and a host moves one across threads: `deno/ext/web`'s broadcast
/// channel carries a `SharedRef<BackingStore>` to the agents that rebuild their
/// own `SharedArrayBuffer`s from it. `Arc` gives that for free and only where it
/// is true — `SharedRef<T>` is `Send + Sync` exactly when `T` is, so nothing is
/// asserted here that the payload does not already promise. Which is why this
/// crate builds the engine's thread-safe block (`Cargo.toml` requests
/// `runtime/workers`): a `BackingStore` is the payload that gets shared, and its
/// bytes are only `Send` in that build.
pub struct SharedRef<T>(Arc<T>);

impl<T> SharedRef<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Arc::new(value))
    }
}

impl<T> Clone for SharedRef<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Deref for SharedRef<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> fmt::Debug for SharedRef<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SharedRef(..)")
    }
}

impl<T> From<UniqueRef<T>> for SharedRef<T> {
    fn from(unique: UniqueRef<T>) -> Self {
        unique.make_shared()
    }
}

/// A pointer to something the crate we stand in for allocates on its own heap,
/// which may be null (`v8::UniquePtr`).
///
/// There is no second heap here: this is the nullable form of [`UniqueRef`], so
/// `None` is the null pointer and `Some` is the owned value. The raw-pointer
/// conversions there (`from_raw`, `into_raw`) are absent rather than stubbed —
/// there is no C++ allocation to hand out or adopt, and a host that needs them
/// gets a compile error instead of a pointer into a Rust box. `T` is sized here
/// where the crate we stand in for allows unsized types, for the same reason:
/// `UniqueRef` owns its value.
#[repr(transparent)]
#[derive(Debug)]
pub struct UniquePtr<T>(Option<UniqueRef<T>>);

impl<T> UniquePtr<T> {
    /// Whether this is the null pointer (`v8::UniquePtr::is_null`).
    pub fn is_null(&self) -> bool {
        self.0.is_none()
    }

    /// The referenced value, if there is one (`v8::UniquePtr::as_ref`).
    pub fn as_ref(&self) -> Option<&UniqueRef<T>> {
        self.0.as_ref()
    }

    /// The referenced value mutably, if there is one
    /// (`v8::UniquePtr::as_mut`).
    pub fn as_mut(&mut self) -> Option<&mut UniqueRef<T>> {
        self.0.as_mut()
    }

    /// Take the referenced value, leaving the null pointer
    /// (`v8::UniquePtr::take`).
    pub fn take(&mut self) -> Option<UniqueRef<T>> {
        self.0.take()
    }

    /// The referenced value, panicking when there is none
    /// (`v8::UniquePtr::unwrap`).
    pub fn unwrap(self) -> UniqueRef<T> {
        self.0.expect("UniquePtr::unwrap on a null pointer")
    }
}

impl<T> Default for UniquePtr<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T> From<UniqueRef<T>> for UniquePtr<T> {
    fn from(unique_ref: UniqueRef<T>) -> Self {
        Self(Some(unique_ref))
    }
}

/// The bytes behind an `ArrayBuffer` (`v8::BackingStore`).
///
/// A store is the engine's byte block plus the geometry the buffer declared
/// when the store was taken: its length, and whether JavaScript may resize it.
/// A buffer's store outlives detaching in this shape, as it does there — a
/// detached buffer hands out a store of zero length rather than nothing.
pub struct BackingStore {
    block: SharedBuffer,
    byte_length: usize,
    resizable_by_user_javascript: bool,
}

impl BackingStore {
    /// A fresh zeroed store (`v8::ArrayBuffer::new_backing_store`).
    pub(crate) fn new(byte_length: usize) -> Self {
        Self {
            block: SharedBuffer::new(byte_length),
            byte_length,
            resizable_by_user_javascript: false,
        }
    }

    /// A store that owns `bytes`, which is the engine's block given the bytes
    /// rather than a Rust allocation adopted as one: the block is the storage
    /// JavaScript reads, so it has to be that block either way.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        let store = Self::new(bytes.len());
        store
            .block
            .write(0, bytes)
            .expect("bridge bug: a fresh store holds what it was given");
        store
    }

    pub(crate) fn from_buffer(
        block: SharedBuffer,
        byte_length: usize,
        resizable_by_user_javascript: bool,
    ) -> Self {
        Self {
            block,
            byte_length,
            resizable_by_user_javascript,
        }
    }

    /// The first byte, or `None` for a store of zero length
    /// (`v8::BackingStore::data`).
    ///
    /// The pointer is the block's live base. A caller writing through it must
    /// hold no borrow of the block across the write, and must not keep it
    /// across a resize — which is why the callers that check
    /// [`is_resizable_by_user_javascript`](Self::is_resizable_by_user_javascript)
    /// are the ones that hold a pointer for any length of time.
    pub fn data(&self) -> Option<NonNull<c_void>> {
        if self.byte_length == 0 {
            return None;
        }
        NonNull::new(self.block.data_ptr().cast::<c_void>())
    }

    /// The length in bytes (`v8::BackingStore::byte_length`).
    pub fn byte_length(&self) -> usize {
        self.byte_length
    }

    /// The same length, under the name the crate we stand in for reaches it
    /// by: it derefs a store to `[Cell<u8>]`, where this is the slice length.
    pub fn len(&self) -> usize {
        self.byte_length
    }

    pub fn is_empty(&self) -> bool {
        self.byte_length == 0
    }

    /// Whether the store was created for a `SharedArrayBuffer`
    /// (`v8::BackingStore::is_shared`).
    pub fn is_shared(&self) -> bool {
        self.block.is_shared()
    }

    /// Whether JavaScript may resize the buffer
    /// (`v8::BackingStore::is_resizable_by_user_javascript`): a resizable
    /// `ArrayBuffer` or a growable `SharedArrayBuffer`.
    pub fn is_resizable_by_user_javascript(&self) -> bool {
        self.resizable_by_user_javascript
    }

    /// The engine block behind the store, for the bridge's own use.
    pub(crate) fn block(&self) -> &SharedBuffer {
        &self.block
    }
}

/// A host function value (v8's `UnitType`): a function item, or a
/// non-capturing closure, which is zero-sized, so it can be reconstructed from
/// nothing instead of being stored.
///
/// That reconstruction is what lets a host pass `my_op` where a callback is
/// wanted, and it is checked twice: the array index below is `size_of::<T>()`,
/// so a capturing closure fails to build, and [`UnitValue::get`] asserts the
/// same at run time.
pub trait UnitType: Copy + Sized {
    fn get() -> Self {
        UnitValue::<Self>::get()
    }
}

impl<T> UnitType for T where T: Copy + Sized {}

struct UnitValue<T>(std::marker::PhantomData<T>);

impl<T: Copy> Clone for UnitValue<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Copy> Copy for UnitValue<T> {}

impl<T: Copy> UnitValue<T> {
    const SELF: Self = Self::new_checked();

    const fn new_checked() -> Self {
        // The index is the compile-time assertion: it is only in range for a
        // zero-sized `T`.
        let value = Self(std::marker::PhantomData);
        [value][size_of::<T>()]
    }

    fn get() -> T {
        // Reading `SELF` is what makes its compile-time check part of this
        // function; the assertion is the run-time backup for it.
        let checked = Self::SELF;
        assert_eq!(size_of_val(&checked), 0, "not a unit function value");
        // SAFETY: `T` is zero-sized, so it carries no information and any bit
        // pattern — including this all-zero one — is a value of it.
        unsafe { std::mem::MaybeUninit::<T>::zeroed().assume_init() }
    }
}

/// The default conversion tag (v8's `DefaultTag`).
pub struct DefaultTag;

/// How a host function becomes the callback a host asks for (v8's `MapFnTo`).
pub trait MapFnTo<T, Tag = DefaultTag>
where
    Self: UnitType,
    T: Sized,
{
    fn mapping() -> T;

    fn map_fn_to(self) -> T {
        Self::mapping()
    }
}

impl<F, T, Tag> MapFnTo<T, Tag> for F
where
    F: UnitType,
    T: MapFnFrom<F, Tag>,
{
    fn mapping() -> T {
        T::map_fn_from(F::get())
    }
}

/// A callback type the bridge knows how to build from a host function
/// (v8's `MapFnFrom`).
pub trait MapFnFrom<F, Tag = DefaultTag>
where
    F: UnitType,
    Self: Sized,
{
    fn mapping() -> Self;

    fn map_fn_from(_: F) -> Self {
        Self::mapping()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boundary's `SharedRef` is thread-safe, which is a *build*
    /// configuration and not just a type: `deno/ext/web`'s broadcast channel
    /// moves a `SharedRef<BackingStore>` to the agents that rebuild their own
    /// `SharedArrayBuffer`s from it, and that only types-checks when the store's
    /// bytes are the engine's thread-safe block. So the claim is asserted here —
    /// reverting `SharedRef` to `Rc`, or this crate's `runtime/workers`, stops
    /// this test compiling.
    #[test]
    fn a_backing_store_reference_can_cross_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SharedRef<BackingStore>>();
    }

    #[test]
    fn a_unique_ptr_is_the_nullable_form_of_a_unique_ref() {
        let mut null: UniquePtr<u32> = UniquePtr::default();
        assert!(null.is_null());
        assert!(null.as_ref().is_none());
        assert!(null.as_mut().is_none());
        assert!(null.take().is_none());

        let mut present = UniquePtr::from(UniqueRef::new(7u32));
        assert!(!present.is_null());
        assert_eq!(**present.as_ref().expect("ref"), 7);
        **present.as_mut().expect("mut") = 8;
        assert_eq!(**present.as_ref().expect("ref"), 8);
        assert_eq!(*present.unwrap(), 8);
    }

    #[test]
    #[should_panic(expected = "UniquePtr::unwrap on a null pointer")]
    fn unwrapping_the_null_pointer_refuses() {
        UniquePtr::<u32>::default().unwrap();
    }
}
