//! A variable-length payload that lives in the same GC box as its header.
//!
//! [`Gc::new`](crate::heap::Gc::new) sizes a box from `size_of::<T>()`, so a
//! payload that needs a variable number of elements — array elements,
//! environment slots, a property vector — otherwise becomes a second
//! allocation behind a pointer (a `Vec`, or a separate `Gc<ArraySlots>`).
//! `InlineArray` is the escape hatch: the box is allocated with its elements
//! in trailing storage, so a handle is one arena slot and an element is one
//! indexed load from it.
//!
//! The elements sit after the header and outside the type's own `size_of`.
//! They are written once by the allocator
//! ([`Gc::with_len`](crate::heap::Gc::with_len)) and reached through
//! [`InlineArray::ELEMENTS_OFFSET`]; the `data` field is a zero-length array
//! and has no storage of its own.

use crate::heap::{GcAny, Trace};

/// A `len`-element array of `T` stored in trailing storage inside the box that
/// owns this header.
///
/// Elements are initialized at allocation and never move: an `InlineArray`
/// cannot be resized, only replaced by a new box. A consumer that appends
/// allocates with room to spare and tracks its own length, the way the dense
/// array's element cursor does.
#[repr(C)]
pub struct InlineArray<T> {
    len: usize,
    /// The elements. Zero elements are laid out here so the type stays sized;
    /// the storage for them is the box's trailing bytes (see
    /// [`InlineArray::ELEMENTS_OFFSET`]), never this field.
    data: [T; 0],
}

impl<T> InlineArray<T> {
    /// Byte offset of the first element from the array's own address.
    ///
    /// `repr(C)` starts the elements at the next multiple of `align_of::<T>()`
    /// after `len`. The allocator refuses an element type wider than
    /// [`ARENA_GRANULARITY`](crate::heap) so that offset is always one
    /// `usize`.
    pub const ELEMENTS_OFFSET: usize = std::mem::size_of::<usize>();

    /// The number of initialized elements.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The elements.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: the box reserved `self.len` elements at `ELEMENTS_OFFSET` and
        // the allocator initialized every one of them; they live in this
        // header's own box, so they outlive the borrow.
        unsafe { std::slice::from_raw_parts(Self::elements_ptr(self), self.len) }
    }

    /// The elements, mutably. `&mut self` is the only route to an element
    /// write; a consumer that appends in place takes the raw pointer from
    /// [`InlineArray::elements_ptr_mut`] instead (the cursor discipline the
    /// dense array uses).
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, and `&mut self` grants unique access to the
        // box the elements live in.
        unsafe { std::slice::from_raw_parts_mut(Self::elements_ptr_mut(self), self.len) }
    }

    /// The element at `index`, or `None` when out of bounds.
    #[inline]
    pub fn get(&self, index: usize) -> Option<&T> {
        self.as_slice().get(index)
    }

    #[inline]
    fn elements_ptr(this: *const Self) -> *const T {
        (this as *const u8).wrapping_add(Self::ELEMENTS_OFFSET) as *const T
    }

    /// The first element's address, for an appending consumer's cursor.
    ///
    /// # Safety
    /// `this` must point at a live payload of a box that reserved at least
    /// `len` elements.
    #[inline]
    pub unsafe fn elements_ptr_mut(this: *mut Self) -> *mut T {
        (this as *mut u8).wrapping_add(Self::ELEMENTS_OFFSET) as *mut T
    }

    /// Write the element count into an uninitialized payload. The allocator's
    /// first initialization step; the elements follow it.
    ///
    /// # Safety
    /// `this` must point at a fresh arena slot sized for the header plus
    /// `len` elements, and every element must be written before the box can
    /// be observed by a collection or a drop.
    pub(crate) unsafe fn write_len(this: *mut Self, len: usize) {
        // SAFETY: `this` is a fresh slot; the field projection stays a place
        // expression (no `&mut` is created over uninitialized memory).
        unsafe { std::ptr::addr_of_mut!((*this).len).write(len) }
    }
}

impl<T> Drop for InlineArray<T> {
    fn drop(&mut self) {
        // SAFETY: the allocator initialized `len` elements at
        // `ELEMENTS_OFFSET`, in this box's own trailing storage, which is
        // still allocated — the sweep drops the payload before the slot is
        // reused.
        unsafe { std::ptr::drop_in_place(self.as_mut_slice()) }
    }
}

impl<T: Trace> Trace for InlineArray<T> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        for element in self.as_slice() {
            element.trace(visit);
        }
    }

    const DROP_MAY_RELEASE: bool = T::DROP_MAY_RELEASE;

    fn drop_releases_heap(&self) -> bool {
        // Consulted only when `T::DROP_MAY_RELEASE` is false, which is that
        // type's promise that no instance owns Rust-heap storage — so no
        // element can own any either.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{Gc, GcCell, with_heap, with_heap_mut};

    #[derive(Default)]
    struct Node {
        next: GcCell<Option<Gc<Node>>>,
    }

    impl Trace for Node {
        fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
            self.next.trace(visit);
        }
    }

    /// An eight-byte element with no edges: the shape an element slot has.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Slot(u64);

    impl Trace for Slot {
        fn trace(&self, _visit: &mut dyn FnMut(GcAny)) {}
    }

    #[test]
    fn elements_live_in_the_box_that_owns_them() {
        let before = with_heap(|heap| heap.allocated_bytes());
        // 16-byte box header + 8-byte length + 5 * 8-byte elements = 64 bytes,
        // one arena slot (the layout is 16-byte granular, so this is exact).
        let array = Gc::<InlineArray<Slot>>::with_len(5, |index| Slot(index as u64));
        let after = with_heap(|heap| heap.allocated_bytes());
        assert_eq!(
            after - before,
            64,
            "the elements are paid for by their own box, not a second one"
        );
        assert_eq!(array.len(), 5);
        assert_eq!(
            array.as_slice(),
            &[Slot(0), Slot(1), Slot(2), Slot(3), Slot(4)]
        );
    }

    #[test]
    fn elements_are_traced_through_the_array() {
        let start = with_heap(|heap| heap.live_count());
        let child = Gc::new(Node::default());
        let array = Gc::<InlineArray<Gc<Node>>>::with_len(2, |_| child);
        assert_eq!(with_heap(|heap| heap.live_count()), start + 2);
        with_heap_mut(|heap| heap.collect(&[array.as_any()]));
        assert_eq!(
            with_heap(|heap| heap.live_count()),
            start + 2,
            "the element is reachable through the array"
        );
        with_heap_mut(|heap| heap.collect(&[]));
        assert_eq!(
            with_heap(|heap| heap.live_count()),
            start,
            "nothing else holds the child"
        );
    }

    #[test]
    fn elements_are_dropped_with_their_box() {
        struct Counted(std::rc::Rc<std::cell::Cell<usize>>);

        impl Trace for Counted {
            fn trace(&self, _visit: &mut dyn FnMut(GcAny)) {}
        }

        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let dropped = std::rc::Rc::new(std::cell::Cell::new(0));
        Gc::<InlineArray<Counted>>::with_len(3, |_| Counted(dropped.clone()));
        // A handle is a plain pointer: the payload is dropped by the sweep,
        // not by the handle going out of scope.
        assert_eq!(dropped.get(), 0);
        with_heap_mut(|heap| heap.collect(&[]));
        assert_eq!(dropped.get(), 3, "every element is dropped with its box");
    }
}
