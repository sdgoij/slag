//! The GC heap (.notes/gc-plan.md, GC-1): `Gc<T>` handles into a thread-local
//! mark-sweep heap.
//!
//! Modeled on the `gc` crate: each traced object lives in a `GcBox<T>` that
//! is individually heap-allocated. `Gc<T>` derefs directly to its box's
//! payload (no arena lookup), so existing `handle.field` call sites survive
//! the migration from `Handle<T> = Rc<T>`.
//!
//! A0 (.notes/nursery-gc-plan.md) removed the per-allocation `live` registry:
//! the collector now enumerates boxes by walking the chunked bump arena, and
//! `Gc::new` no longer pays a registry push. Every slot keeps a valid `size`
//! (a swept slot's live bit is cleared, so the walk steps over it exactly),
//! and every box header carries a per-type vtable, so the erased walk can
//! trace and drop a box it only knows by address.
//!
//! Soundness invariant: **every live `Gc<T>` must be reachable from the roots
//! passed to [`Heap::collect`]** (or from a conservative stack scan, which the
//! arena refinement in GC-1 adds). A `Gc<T>` that is unmarked at sweep time
//! is dropped while the handle still exists — a use-after-free. The
//! `--gc-stress` mode (collect on every allocation) is the test net for this.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::rc::Rc;

/// A value whose object graph the collector can trace. Fields holding `Gc<T>`
/// must visit them in `trace`.
pub trait Trace: 'static {
    fn trace(&self, visit: &mut dyn FnMut(GcAny));

    /// Report that a young value was stored at slot `index` of this box (a
    /// dense array's element buffer). The write barrier calls this on its
    /// old-target path only, so a type whose slots are addressable can lower a
    /// low-water mark and let [`Trace::trace_dirty`] visit just the slots
    /// written since the last collection. The default does nothing: for a box
    /// whose children are a handful of fields, tracing the box is already the
    /// bound.
    fn note_dirty_slot(&self, _index: u32) {}

    /// Visit the children a *minor* collection must see. The default is the
    /// full trace, which is always sound; a type that can bound its dirty slots
    /// overrides it. A collector calls this instead of `trace` on an **old** box
    /// — never on a young one, whose children may have been stored before it was
    /// old (the barrier does not note a young target).
    ///
    /// A bound must be a superset of the box's young children, or a young value
    /// goes unmarked and the minor sweeps it (`--gc-verify` is the net for
    /// that).
    fn trace_dirty(&self, visit: &mut dyn FnMut(GcAny)) {
        self.trace(visit);
    }
}

/// An erased GC reference: the header address of any `Gc<T>`. Produced by
/// `Gc<T>`'s `Trace` impl and consumed as a [`Heap::collect`] root. Thin —
/// the per-type vtable lives in the box header, so the arena walk can trace a
/// box it only knows by address.
#[derive(Clone, Copy)]
pub struct GcAny(*mut GcHeader);

impl GcAny {
    /// The box base address (the identity used by the conservative stack
    /// scan and the weak-table compaction after a collection).
    pub fn addr(self) -> usize {
        self.0 as usize
    }

    /// Whether the box's mark bit is set: reachable from the roots of the
    /// collection whose mark phase just finished. Read only between the mark
    /// and the sweep (unmarked boxes are dropped right after).
    pub(crate) fn is_marked(self) -> bool {
        // SAFETY: `self` comes from a live box (`Gc<T>`'s `Trace`); the header
        // is valid from the mark phase until the sweep resets it.
        unsafe { (*self.0).is_marked() }
    }

    pub(crate) fn set_marked(self, marked: bool) {
        // SAFETY: as `is_marked`.
        unsafe { (*self.0).set_marked(marked) };
    }

    /// Whether the box is young (allocated since the last collection).
    pub(crate) fn is_young(self) -> bool {
        // SAFETY: `self` comes from a live box.
        unsafe { (*self.0).is_young() }
    }

    /// Whether the box's slot is live. Read by the cache-prune regression test
    /// (`map.rs`), which asserts a pruned entry yields a live map.
    #[cfg(test)]
    pub(crate) fn is_live(self) -> bool {
        // SAFETY: `self` comes from a live box; the header outlives the payload
        // as a free-list slot.
        unsafe { (*self.0).is_live() }
    }

    /// Trace the box's outgoing edges.
    ///
    /// SAFETY: the box must be live (the rooting discipline).
    pub(crate) unsafe fn trace(self, visit: &mut dyn FnMut(GcAny)) {
        // SAFETY: the caller guarantees `self.0` is a live box header.
        unsafe {
            let header = &*self.0;
            (header.vtable.trace)(header.data_ptr(self.0), visit);
        }
    }

    /// Trace the children a minor collection must see (A5.1): the full set for
    /// a type that does not bound its dirty slots, the slots written since the
    /// last collection for one that does.
    ///
    /// SAFETY: as `trace`; additionally the box must be **old**, since a young
    /// box's children may predate its promotion and its dirty mark would then
    /// be empty.
    pub(crate) unsafe fn trace_dirty(self, visit: &mut dyn FnMut(GcAny)) {
        // SAFETY: as `trace`.
        unsafe {
            let header = &*self.0;
            (header.vtable.trace_dirty)(header.data_ptr(self.0), visit);
        }
    }
}

/// A box's liveness: whether this arena slot currently holds a live box (the
/// arena walk skips the cleared slots).
const FLAG_LIVE: u32 = 1 << 0;
/// The collector's mark.
const FLAG_MARK: u32 = 1 << 1;
/// The box's generation (A1): set at allocation, cleared when the box survives
/// a collection. "Young" means allocated since the last collection; the young
/// bit is authoritative (promotion is in place, so an address range cannot
/// stand in for it).
const FLAG_YOUNG: u32 = 1 << 2;
/// The box is in the remembered set (A2): it is old and a young value was
/// stored into it, so a minor collection must trace it. The bit is the
/// set's dedup index (the barrier cannot afford a hash lookup per store).
const FLAG_REMEMBERED: u32 = 1 << 3;

/// The per-type entry points stored in every box header, so the arena walk
/// can trace and drop a box it only knows by address.
pub(crate) struct VTable {
    trace: fn(*const u8, &mut dyn FnMut(GcAny)),
    /// A5.1: [`Trace::trace_dirty`] — the minor's bounded trace for an old box.
    trace_dirty: fn(*const u8, &mut dyn FnMut(GcAny)),
    /// A5.1: [`Trace::note_dirty_slot`] — the barrier's slot report on the
    /// old-target path.
    note_dirty: fn(*const u8, u32),
    drop: fn(*mut u8),
    /// The payload's offset within its box — fixed by the header layout (see
    /// [`GcHeader`]), so the walk reads it rather than assuming.
    data_offset: u32,
    /// The payload's type name, for diagnostics (the A2 barrier verifier names
    /// the containers it finds an unrecorded young reference in). A function
    /// pointer because `type_name` is not const-stable.
    name: fn() -> &'static str,
}

/// [`VTable::name`]: the payload's type name.
fn type_name_of<T>() -> &'static str {
    std::any::type_name::<T>()
}

/// The header every box begins with. `repr(C)` and first in [`GcBox`], so a
/// box address is also a header address.
#[repr(C)]
struct GcHeader {
    flags: Cell<u32>,
    /// The box's total arena footprint (header + data, rounded to
    /// [`ARENA_GRANULARITY`]), written once at allocation. The arena walk
    /// steps by it — including across swept slots.
    size: u32,
    vtable: &'static VTable,
}

impl GcHeader {
    #[inline]
    fn is_live(&self) -> bool {
        self.flags.get() & FLAG_LIVE != 0
    }

    #[inline]
    fn set_live(&self, live: bool) {
        let flags = self.flags.get();
        self.flags.set(if live {
            flags | FLAG_LIVE
        } else {
            flags & !FLAG_LIVE
        });
    }

    #[inline]
    fn is_marked(&self) -> bool {
        self.flags.get() & FLAG_MARK != 0
    }

    #[inline]
    fn set_marked(&self, marked: bool) {
        let flags = self.flags.get();
        self.flags.set(if marked {
            flags | FLAG_MARK
        } else {
            flags & !FLAG_MARK
        });
    }

    /// Whether the box is young (allocated since the last collection).
    ///
    /// Read by the write barrier (A2), the debug young-drain check, and tests.
    #[inline]
    fn is_young(&self) -> bool {
        self.flags.get() & FLAG_YOUNG != 0
    }

    #[inline]
    fn is_remembered(&self) -> bool {
        self.flags.get() & FLAG_REMEMBERED != 0
    }

    #[inline]
    fn set_remembered(&self, remembered: bool) {
        let flags = self.flags.get();
        self.flags.set(if remembered {
            flags | FLAG_REMEMBERED
        } else {
            flags & !FLAG_REMEMBERED
        });
    }

    #[inline]
    fn set_young(&self, young: bool) {
        let flags = self.flags.get();
        self.flags.set(if young {
            flags | FLAG_YOUNG
        } else {
            flags & !FLAG_YOUNG
        });
    }

    /// The address of the payload this header belongs to.
    #[inline]
    fn data_ptr(&self, base: *mut GcHeader) -> *mut u8 {
        (base as *mut u8).wrapping_add(self.vtable.data_offset as usize)
    }
}

/// A cell in the GC heap. `T` is unsized only through `dyn Trace`; for a
/// typed `Gc<T>` the box is sized. The header (liveness flags, the box's
/// rounded arena size, the type's vtable) precedes the payload the handle
/// derefs to.
#[repr(C)]
struct GcBox<T: ?Sized + Trace> {
    header: GcHeader,
    data: T,
}

impl<T: Trace> GcBox<T> {
    /// The type's vtable: how the erased walk traces and drops this box.
    const VTABLE: &'static VTable = &VTable {
        trace: |data, visit| {
            // SAFETY: `data` points at this box's `T` payload.
            unsafe { (*data.cast::<T>()).trace(visit) }
        },
        trace_dirty: |data, visit| {
            // SAFETY: as above.
            unsafe { (*data.cast::<T>()).trace_dirty(visit) }
        },
        note_dirty: |data, index| {
            // SAFETY: as above.
            unsafe { (*data.cast::<T>()).note_dirty_slot(index) }
        },
        drop: |data| {
            // SAFETY: `data` points at this box's `T` payload, dropped once
            // by the sweep before the slot is reused.
            unsafe { std::ptr::drop_in_place(data.cast::<T>()) }
        },
        data_offset: std::mem::offset_of!(GcBox<T>, data) as u32,
        name: type_name_of::<T>,
    };
}

/// The offset of a boxed value's data within its `GcBox` (the box header
/// precedes the value): the compiled member-cell probe adds this to the
/// NaN-boxing payload's box base to reach the `JsObject`. The header fields
/// are fixed, so the offset is a stable ABI constant.
pub const GCBOX_DATA_OFFSET: usize = std::mem::offset_of!(GcBox<crate::Value>, data);

/// The `flags` bit meaning "allocated since the last collection" (A1), at the
/// box's offset 0. The JIT reads it to decide whether an inline heap store
/// needs the A2 write barrier: a young container cannot hold an old->young
/// edge, so it stores inline; an old one bails to the helper, which runs the
/// barrier.
pub const GC_FLAG_YOUNG: u32 = FLAG_YOUNG;

/// A heap handle: a `Copy` pointer into the GC heap. `!Send`/`!Sync` by the
/// raw-pointer marker — a heap is agent-local (workers use separate agents).
/// `T` may be unsized (`dyn HostOps` for host-defined exotics); `Gc::new`
/// requires a sized `T`.
///
/// Equality forwards to the pointee (like `Rc`), so `ValueKind`'s derived
/// `PartialEq` keeps its old semantics: strings compare by content, objects
/// by identity (their `PartialEq` is id-based).
pub struct Gc<T: ?Sized + Trace> {
    ptr: NonNull<GcBox<T>>,
    _not_send_sync: std::marker::PhantomData<*mut ()>,
}

impl<T: ?Sized + Trace + PartialEq> PartialEq for Gc<T> {
    fn eq(&self, other: &Gc<T>) -> bool {
        **self == **other
    }
}
impl<T: ?Sized + Trace + Eq> Eq for Gc<T> {}
impl<T: ?Sized + Trace + std::hash::Hash> std::hash::Hash for Gc<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Hash the pointee, not the pointer: a handle to an equal value must
        // hash like the value (PropertyKey's derived Hash relies on this).
        (**self).hash(state);
    }
}

impl<T: ?Sized + Trace> Copy for Gc<T> {}
impl<T: ?Sized + Trace> Clone for Gc<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized + Trace> AsRef<T> for Gc<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: Trace> Gc<T> {
    /// The erased form, usable as a [`Heap::collect`] root.
    pub fn as_any(self) -> GcAny {
        GcAny(self.ptr.as_ptr() as *mut GcHeader)
    }

    /// Pointer identity, replacing `Rc::ptr_eq` for the migration.
    pub fn ptr_eq(self, other: Gc<T>) -> bool {
        self.ptr == other.ptr
    }
}

impl<T: Trace> Gc<T> {
    /// Allocate a new box in the bump arena. A5.1: replaces the per-box
    /// `Box::new` malloc with a bump + size-classed free-list reuse inside
    /// one heap borrow.
    pub fn new(value: T) -> Gc<T> {
        let size = round_up(size_of::<GcBox<T>>(), ARENA_GRANULARITY);
        let gc = with_heap_mut(|heap| {
            let raw = heap.alloc(size, align_of::<GcBox<T>>()) as *mut GcBox<T>;
            // SAFETY: `raw` is a fresh arena slot (bumped or reused) of at
            // least `size` bytes; writing the header + payload initializes
            // the box before any handle can see it.
            unsafe {
                raw.write(GcBox {
                    header: GcHeader {
                        flags: Cell::new(FLAG_LIVE | FLAG_YOUNG),
                        size: size as u32,
                        vtable: GcBox::<T>::VTABLE,
                    },
                    data: value,
                });
            }
            heap.note_alloc(raw as usize);
            Gc {
                ptr: unsafe { NonNull::new_unchecked(raw) },
                _not_send_sync: std::marker::PhantomData,
            }
        });
        ALLOC_SINCE_COLLECT.with(|count| count.set(count.get() + 1));
        // GC-2 `--gc-stress`: the fresh box is not yet reachable from any
        // handle the caller holds, so it is passed through as an extra root.
        maybe_stress_collect(gc.as_any());
        gc
    }

    /// Allocate a box and initialize its payload in place. `init` writes the
    /// value directly into the arena slot, so a large payload (the 528B
    /// `JsObject`) skips the stack-temp build + memcpy that `Gc::new` pays —
    /// the hot allocation paths (object literals, construct churn) measured
    /// ~80ns of that copy per allocation. `init` MUST write every field of
    /// `*T` before returning (the slot starts uninitialized); it must not
    /// allocate through the heap (the heap is mutably borrowed here).
    pub fn new_in_place(init: impl FnOnce(*mut T)) -> Gc<T> {
        let size = round_up(size_of::<GcBox<T>>(), ARENA_GRANULARITY);
        let gc = with_heap_mut(|heap| {
            let raw = heap.alloc(size, align_of::<GcBox<T>>()) as *mut GcBox<T>;
            // SAFETY: `raw` is a fresh arena slot (bumped or reused) of at
            // least `size` bytes; the header is written here and `init`
            // initializes the payload before any handle can see it.
            unsafe {
                let boxed = &mut *raw;
                boxed.header.flags = Cell::new(FLAG_LIVE | FLAG_YOUNG);
                boxed.header.size = size as u32;
                boxed.header.vtable = GcBox::<T>::VTABLE;
                init(std::ptr::addr_of_mut!(boxed.data));
            }
            heap.note_alloc(raw as usize);
            Gc {
                ptr: unsafe { NonNull::new_unchecked(raw) },
                _not_send_sync: std::marker::PhantomData,
            }
        });
        ALLOC_SINCE_COLLECT.with(|count| count.set(count.get() + 1));
        maybe_stress_collect(gc.as_any());
        gc
    }

    /// The box base address, for NaN-boxing into `Value`'s 44-bit payload.
    pub(crate) fn box_ptr(self) -> usize {
        self.ptr.as_ptr() as usize
    }

    /// A raw pointer to the boxed value, replacing `Rc::as_ptr` (used as an
    /// identity key). Valid while the box is live.
    pub fn as_ptr(self) -> *const T {
        &*self as *const T
    }

    /// Reconstruct a handle from a box base address produced by `box_ptr`.
    ///
    /// SAFETY: `ptr` must be a live `GcBox<T>` in the current thread's heap
    /// (the rooting discipline guarantees this for every encoded value).
    pub(crate) unsafe fn from_box_ptr(ptr: usize) -> Gc<T> {
        // SAFETY (caller): `ptr` is a live `GcBox<T>`; `new_unchecked` trusts
        // the non-null invariant the rooting discipline guarantees.
        unsafe {
            Gc {
                ptr: NonNull::new_unchecked(ptr as *mut GcBox<T>),
                _not_send_sync: std::marker::PhantomData,
            }
        }
    }

    /// The handle to the box that owns `payload` — a reference into a box's
    /// payload, used to build a handle to `self` inside a method (e.g. a
    /// `Map`'s back-pointer to its parent). The offset comes from *this*
    /// type's layout, so it cannot drift when the header changes; never
    /// hardcode it.
    ///
    /// SAFETY: `payload` must be the payload of a live box (a reference
    /// obtained by dereferencing a live handle, not a stack or free value).
    pub(crate) unsafe fn from_payload(payload: &T) -> Gc<T> {
        let base =
            (payload as *const T as usize).wrapping_sub(std::mem::offset_of!(GcBox<T>, data));
        // SAFETY (caller): `payload` lives in a live box, so `base` is that
        // box's address.
        unsafe { Gc::from_box_ptr(base) }
    }
}

impl<T: ?Sized + Trace> Deref for Gc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // The box is kept alive by the rooting discipline (see the module
        // docs): a live handle must be marked at sweep time.
        unsafe { &self.ptr.as_ref().data }
    }
}

impl<T: ?Sized + Trace> DerefMut for Gc<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut self.ptr.as_mut().data }
    }
}

impl<T: Trace> Trace for Gc<T> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        visit(self.as_any());
    }
}

impl<T: ?Sized + Trace> fmt::Debug for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug on handles must not require `T: Debug` (derived Debug impls on
        // structs containing handles — Realm, EnvRecord, ... — would break).
        write!(f, "Gc({:p})", self.ptr.as_ptr())
    }
}

impl<T: ?Sized + Trace + fmt::Display> fmt::Display for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// Interior mutability for fields inside GC objects. The `RefCell` borrow is
/// only read during tracing (the collector marks; it never mutates payloads).
pub struct GcCell<T> {
    inner: RefCell<T>,
}

impl<T> GcCell<T> {
    pub fn new(value: T) -> GcCell<T> {
        GcCell {
            inner: RefCell::new(value),
        }
    }
}

impl<T: Trace> Trace for GcCell<T> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        // `RefCell<T>`'s own trace skips the cell and aborts the sweep when
        // it is mutably borrowed mid-collection (per-allocation
        // `--gc-stress`), instead of panicking.
        self.inner.trace(visit);
    }
}

impl<T> Deref for GcCell<T> {
    type Target = RefCell<T>;
    fn deref(&self) -> &RefCell<T> {
        &self.inner
    }
}

impl<T> DerefMut for GcCell<T> {
    fn deref_mut(&mut self) -> &mut RefCell<T> {
        &mut self.inner
    }
}

impl<T: Trace> Trace for Option<T> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        if let Some(value) = self {
            value.trace(visit);
        }
    }
}

impl<T: Trace> Trace for Vec<T> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        for value in self {
            value.trace(visit);
        }
    }
}

impl<T: Trace> Trace for VecDeque<T> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        for value in self {
            value.trace(visit);
        }
    }
}

impl<K: Eq + std::hash::Hash + 'static, V: Trace, S: std::hash::BuildHasher + 'static> Trace
    for HashMap<K, V, S>
{
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        for value in self.values() {
            value.trace(visit);
        }
    }
}

impl<T: Trace> Trace for RefCell<T> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        // A collection can run mid-mutation (per-allocation `--gc-stress`):
        // a borrowed cell cannot be read without panicking. Skipping it and
        // aborting the sweep (retain everything) is safe — imprecise, never
        // a use-after-free; the collector retries at the next safe point.
        match self.try_borrow() {
            Ok(guard) => guard.trace(visit),
            Err(_) => note_aborted_trace(),
        }
    }
}

impl<T: Trace + Copy> Trace for Cell<T> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        // `Cell` carries no borrow state, so the value is always readable
        // mid-collection (unlike `RefCell`, no abort path needed).
        self.get().trace(visit);
    }
}

impl<T: Trace> Trace for Rc<T> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.as_ref().trace(visit);
    }
}

impl<A: Trace, B: Trace> Trace for (A, B) {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.0.trace(visit);
        self.1.trace(visit);
    }
}

impl<A: Trace, B: Trace, C: Trace> Trace for (A, B, C) {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.0.trace(visit);
        self.1.trace(visit);
        self.2.trace(visit);
    }
}

impl<A: Trace, B: Trace, C: Trace, D: Trace> Trace for (A, B, C, D) {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.0.trace(visit);
        self.1.trace(visit);
        self.2.trace(visit);
        self.3.trace(visit);
    }
}

impl<T: Default> Default for GcCell<T> {
    fn default() -> Self {
        GcCell {
            inner: RefCell::new(T::default()),
        }
    }
}

/// The fixed size of an arena chunk (1 MiB). Chunks are appended as the
/// bump grows; each chunk's buffer is a `Box<[u8]>` whose address never
/// moves, so box addresses stay stable across chunk growth.
const ARENA_CHUNK_SIZE: usize = 1 << 20;

/// The granularity every box's arena footprint is rounded up to. A fixed
/// multiple keeps the bump pointer aligned for any box whose alignment is
/// ≤ this value and makes the arena walk (step by `size`) exact.
const ARENA_GRANULARITY: usize = 16;

/// A contiguous arena chunk: an uninitialized buffer plus a bump pointer.
struct ArenaChunk {
    /// Owns the chunk's memory (never read — boxes are addressed directly).
    #[allow(dead_code)]
    data: Box<[u8]>,
    /// The first box slot (the buffer start, rounded to
    /// [`ARENA_GRANULARITY`]); the arena walk starts here.
    start: usize,
    /// The next allocation offset within the chunk.
    bump: usize,
    /// The last usable address (exclusive).
    end: usize,
}

/// The number of free-list size classes: one per 16-byte rounded size from
/// 16 to `FREE_CLASSES * 16` (4096). A direct-mapped array — the hot
/// allocation path indexes by `size >> 4` instead of hashing (the FxHash
/// HashMap lookup was ~10ns/alloc on the construct bench). Boxes larger
/// than the last class bump-allocate and their slots are not reused (rare).
const FREE_CLASSES: usize = 256;

/// The free-list slot index for a rounded box size, or `None` when the
/// size exceeds the classes (those boxes' slots are not reclaimed).
#[inline]
fn free_index(size: usize) -> Option<usize> {
    (ARENA_GRANULARITY..=FREE_CLASSES * ARENA_GRANULARITY)
        .contains(&size)
        .then(|| size / ARENA_GRANULARITY - 1)
}

/// The thread-local mark-sweep heap.
pub struct Heap {
    /// The bump arena backing every box. Boxes live at stable addresses
    /// inside these chunks.
    chunks: Vec<ArenaChunk>,
    /// Chunk indices sorted by `start`, so the arena walk visits boxes in
    /// ascending address order without sorting them per collection (the
    /// conservative stack scan's list must be sorted). Refreshed by
    /// `push_chunk`.
    order: Vec<usize>,
    /// Reclaimed slots by rounded size class (see [`FREE_CLASSES`]): swept
    /// (dead) boxes are reused by `Gc::new` before the bump advances. Boxes
    /// are never freed individually — the arena keeps the memory, and slots
    /// cycle through the free list.
    free: [Vec<usize>; FREE_CLASSES],
    /// Live boxes, maintained by allocation and the sweep. The growth
    /// trigger (`Agent::maybe_collect`) and the leak harness read it.
    live_boxes: usize,
    /// The young cohort (A1): the boxes allocated since the last collection,
    /// in allocation order. A minor collection (A3) enumerates young boxes
    /// from here, so it costs O(young) rather than O(live); a box that
    /// survives a collection is promoted in place and the list is drained.
    young: Vec<usize>,
    /// `[low, high)` spanning every chunk's allocated slots. Chunks are never
    /// freed, so the bounds only widen; a chunk push updates them. Used as the
    /// conservative stack scan's pre-filter and as the write barrier's
    /// debug-only "is this a box address" check.
    arena_low: usize,
    arena_high: usize,
    /// A5 `--gc-trace`: the stack words the current collection's conservative
    /// scan examined (reset at the start of each collection, set by
    /// [`Heap::scan_stack`]). A `Cell` because `scan_stack` borrows `&self`.
    stack_words: Cell<usize>,
}

/// Round `n` up to the next multiple of `m` (a power of two).
const fn round_up(n: usize, m: usize) -> usize {
    n.div_ceil(m) * m
}

/// Align `n` up to the next multiple of `m` (a power of two).
const fn align_up(n: usize, m: usize) -> usize {
    (n + m - 1) & !(m - 1)
}

/// A fast non-cryptographic hasher for the box-address maps (GC-5): the
/// addresses are word-aligned and not attacker-controlled, so SipHash's
/// collision resistance is wasted cost on every collection's `by_addr`
/// build and the precise dead set. Cut 66: also the shape-transition maps
/// (`Map::transitions`) and the agent's `ecma_functions` table — their keys
/// (atom ids, function ids) are not attacker-controlled either, and the
/// closure-creation path hashes them per property append / per insert.
#[derive(Default)]
pub(crate) struct FxHasher(u64);

impl std::hash::Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.mix(u64::from(byte));
        }
    }
    fn write_u8(&mut self, n: u8) {
        self.mix(u64::from(n));
    }
    fn write_u16(&mut self, n: u16) {
        self.mix(u64::from(n));
    }
    fn write_u32(&mut self, n: u32) {
        self.mix(u64::from(n));
    }
    fn write_u64(&mut self, n: u64) {
        self.mix(n);
    }
    fn write_usize(&mut self, n: usize) {
        self.mix(n as u64);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

impl FxHasher {
    /// The FxHash round for one whole word. The `Hasher` trait's primitive
    /// methods default to `self.write(&n.to_ne_bytes())` (a byte-at-a-time
    /// loop), which made every derived integer hash — an atom-keyed
    /// `Map::transitions` lookup, a GC address — pay 4-8 rounds for one
    /// integer; the whole-word round is one.
    #[inline]
    fn mix(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

type AddrMap = std::collections::HashMap<usize, GcAny, std::hash::BuildHasherDefault<FxHasher>>;

type AddrSet = std::collections::HashSet<usize, std::hash::BuildHasherDefault<FxHasher>>;

thread_local! {
    static HEAP: RefCell<Heap> = const { RefCell::new(Heap::new()) };
    /// GC-5: allocations since the last safe-point check — the cheap
    /// mid-script collection trigger. Loop back-edges cannot afford a
    /// live-count read (TLS + RefCell borrow per iteration), so `Gc::new`
    /// bumps this and the back-edge check compares it against
    /// [`ALLOC_BUDGET`] (one TLS read).
    static ALLOC_SINCE_COLLECT: Cell<u64> = const { Cell::new(0) };
    /// GC-5: safe-point backoff — set to `u64::MAX` when a safe-point
    /// collection swept nothing (a growing live set, e.g. a concat rope,
    /// keeps every node reachable, so re-marking it each budget crossing is
    /// GC-5/A5: whether a major collection is suppressed at safe points. A
    /// major that swept nothing is pure overhead for a growing live set (a
    /// concat rope keeps every node reachable), so it is disabled until the
    /// next script boundary — the same policy GC-5 had, but now applying to
    /// the major level alone.
    ///
    /// The minor level is gated by its own flag, never by this one: a minor
    /// costs O(young + remembered + the roots' fan-out) rather than O(live), and
    /// its job is to keep the young cohort bounded, so the two levels back off
    /// independently (see [`MINOR_DISABLED`]).
    static MAJOR_DISABLED: Cell<bool> = const { Cell::new(false) };
    /// A5: whether a minor collection is suppressed at safe points. Same policy
    /// as the major, one level down: a minor whose cohort was entirely live
    /// reclaimed nothing, so re-walking the roots and re-marking that cohort
    /// every `nursery_threshold` allocations is pure overhead until the next
    /// script boundary. The levels are independent by design — a backing-off
    /// minor does not stall majors (their growth trigger still fires and their
    /// sweep drains the cohort), and a backing-off major does not stall minors.
    ///
    /// This is what keeps the `string concat` row (a 100k-node rope, every node
    /// reachable) off the minor path: without it the row pays a root walk every
    /// 8192 allocations and measured ~20% slower.
    static MINOR_DISABLED: Cell<bool> = const { Cell::new(false) };
    /// A6: boxes the sweeps freed since the compiled safe point last asked (see
    /// [`take_swept_since_check`]). Accumulated by both levels' bookkeeping, so
    /// a sweep from any collection path — a compiled safe point, an interpreter
    /// back edge, a job boundary, or a helper's nested call — is seen.
    static SWEPT_SINCE_CHECK: Cell<usize> = const { Cell::new(0) };
}

/// The allocation budget that paces safe-point collections (GC-5): after
/// this many allocations since the last check, the runtime runs its real
/// collection trigger (`Agent::maybe_collect`, which still gates on the
/// live count). 1024 keeps the heap bounded at a few thousand garbage
/// boxes in a hot allocation loop while the back-edge check itself stays a
/// single compare.
const ALLOC_BUDGET: u64 = 1024;

/// GC-5/A5: the cheap safe-point check for loop back-edges. Returns true when
/// enough allocations have happened since the last check (the caller then runs
/// its per-level trigger). The counter is reset either way. `#[inline]` so the
/// back-edge check is a TLS read + compare (cross-crate calls are not inlined
/// otherwise). The interval is a plain pacing counter: the *levels* decide what
/// to do with the safe point (see [`major_disabled`]), so a backed-off major
/// cannot starve a minor, and vice versa.
#[inline]
pub fn allocation_budget_exceeded() -> bool {
    // Fast path: below the base budget — one TLS read and out (the machinery
    // rows never allocate, so this is the hot shape: counter is 0).
    if ALLOC_SINCE_COLLECT.with(|count| count.get()) < ALLOC_BUDGET {
        return false;
    }
    ALLOC_SINCE_COLLECT.with(|count| count.set(0));
    true
}

/// GC-5/A5: reset the safe-point allocation budget (a script/job boundary —
/// the budget counts allocations since the last trigger or collection, so it
/// must not leak across scripts). Also re-enables the major level that an empty
/// sweep disabled.
pub fn reset_allocation_budget() {
    ALLOC_SINCE_COLLECT.with(|count| count.set(0));
    MAJOR_DISABLED.with(|disabled| disabled.set(false));
    MINOR_DISABLED.with(|disabled| disabled.set(false));
}

/// GC-5: record a collection that ran (any path — a script/job boundary, a
/// safe point, or `--gc-stress`): the allocation budget restarts from zero
/// (so it counts allocations since the last collection, never drifting). A
/// collection that swept nothing is pure overhead for a growing live set (a
/// concat rope keeps every node reachable), so mid-loop collections are
/// disabled until the next script boundary; reclamation keeps them eager.
pub fn note_collection(swept: usize) {
    ALLOC_SINCE_COLLECT.with(|count| count.set(0));
    MAJOR_DISABLED.with(|disabled| disabled.set(swept == 0));
    SWEPT_SINCE_CHECK.with(|count| count.set(count.get() + swept));
}

/// A5: whether the major level is currently suppressed at safe points (an
/// earlier major swept nothing — see [`note_collection`]). The caller must
/// consult this before running a major; the minor level never is.
pub fn major_disabled() -> bool {
    MAJOR_DISABLED.with(|disabled| disabled.get())
}

/// A5: whether the minor level is currently suppressed at safe points (an
/// earlier minor reclaimed nothing — see [`note_minor_collection`]).
pub fn minor_disabled() -> bool {
    MINOR_DISABLED.with(|disabled| disabled.get())
}

/// A5.1: the current collection interval's stamp (see [`COLLECTION_GEN`]). A type
/// bounding its dirty slots stores it beside the bound; the collector reads it to
/// tell a live mark from a stale one.
pub fn collection_generation() -> u32 {
    COLLECTION_GEN.with(|stamp| stamp.get())
}

/// A5.1: close the current interval. Called at the end of every collection
/// (including the aborts, which promote every live box, so no box can hold a young
/// child afterwards either).
fn bump_collection_generation() {
    COLLECTION_GEN.with(|stamp| stamp.set(stamp.get().wrapping_add(1)));
}

/// A3: record a minor collection. The pacing restarts so the next trigger is
/// measured from here; the major level is left exactly as it was (a minor that
/// reclaims nothing must not suppress a major). A minor that reclaimed nothing
/// suppresses the *minor* level until the next script boundary, which is what
/// keeps a growing-live-set loop (a concat rope) off the minor path.
pub fn note_minor_collection(swept: usize) {
    ALLOC_SINCE_COLLECT.with(|count| count.set(0));
    MINOR_DISABLED.with(|disabled| disabled.set(swept == 0));
    SWEPT_SINCE_CHECK.with(|count| count.set(count.get() + swept));
}

/// A6: the boxes the sweeps freed since the last call, resetting the counter.
/// The compiled safe point flushes its cached call-site records only when this
/// is non-zero — a record caches its callee by payload, so a freed box whose
/// address a later allocation recycles could match it and apply a stale verdict.
/// A collection that freed nothing cannot, and with the nursery pacing minors
/// most budget crossings collect nothing, so the unconditional flush cost a
/// re-probe per crossing for no reason.
pub fn take_swept_since_check() -> usize {
    SWEPT_SINCE_CHECK.with(|count| count.take())
}

/// A5 `--gc-trace`: one collection's counters, printed per collection and
/// accumulated for the CLI's exit summary.
#[derive(Clone, Copy, Debug)]
pub struct GcTraceRecord {
    /// `"minor"` or `"major"`.
    pub level: &'static str,
    pub pause_us: u64,
    pub live_before: usize,
    pub live_after: usize,
    pub swept: usize,
    /// The young cohort when the collection started.
    pub young: usize,
    /// The remembered set when the collection started — the barrier's recorded
    /// old->young edges since the last collection.
    pub remembered: usize,
    /// Stack words the conservative scan examined.
    pub stack_words: usize,
}

thread_local! {
    /// A5 `--gc-trace`: one TLS read per collection when off.
    static GC_TRACE: Cell<bool> = const { Cell::new(false) };
    static GC_TRACE_RECORDS: RefCell<Vec<GcTraceRecord>> = const { RefCell::new(Vec::new()) };
}

/// A5 `--gc-trace`: enable per-collection telemetry. Zero cost when off (the
/// collectors read one TLS flag and exit).
pub fn set_gc_trace(enabled: bool) {
    GC_TRACE.with(|flag| flag.set(enabled));
}

/// A5: whether collection telemetry is enabled.
pub fn gc_trace_enabled() -> bool {
    GC_TRACE.with(|flag| flag.get())
}

/// A5 `--gc-trace`: take the accumulated records (the CLI's exit summary).
pub fn take_gc_trace_records() -> Vec<GcTraceRecord> {
    GC_TRACE_RECORDS.with(|records| std::mem::take(&mut *records.borrow_mut()))
}

/// A5 `--gc-trace`: print `record` and keep it for the exit summary.
fn record_gc_trace(record: GcTraceRecord) {
    eprintln!(
        "gc-trace\t{}\tpause_us={}\tlive={}->{}\tswept={}\tyoung={}\tremembered={}\tstack_words={}",
        record.level,
        record.pause_us,
        record.live_before,
        record.live_after,
        record.swept,
        record.young,
        record.remembered,
        record.stack_words,
    );
    GC_TRACE_RECORDS.with(|records| records.borrow_mut().push(record));
}

/// The per-collection state A5's telemetry reports, captured before a
/// collection so the record can be emitted after it.
struct GcTraceStart {
    started: std::time::Instant,
    live_before: usize,
    young: usize,
    remembered: usize,
}

impl Heap {
    /// A5: clear the per-collection scan counter. Called by each entry point
    /// before it (maybe) scans the stack.
    fn reset_trace_start(&self) {
        self.stack_words.set(0);
    }

    /// A5: capture the pre-collection state, or `None` when tracing is off (the
    /// only cost on the hot path is one TLS read).
    fn trace_start(&self) -> Option<GcTraceStart> {
        gc_trace_enabled().then(|| GcTraceStart {
            started: std::time::Instant::now(),
            live_before: self.live_boxes,
            young: self.young.len(),
            remembered: REMEMBERED.with(|set| set.borrow().len()),
        })
    }

    /// A5: emit one collection's record.
    fn trace_end(&self, level: &'static str, start: Option<GcTraceStart>, swept: usize) {
        let Some(start) = start else {
            return;
        };
        record_gc_trace(GcTraceRecord {
            level,
            pause_us: start.started.elapsed().as_micros() as u64,
            live_before: start.live_before,
            live_after: self.live_boxes,
            swept,
            young: start.young,
            remembered: start.remembered,
            stack_words: self.stack_words.get(),
        });
    }
}

/// GC-2 `--gc-stress`: collect after every allocation. The collector runs
// from `Gc::new` with the just-created box as an extra root (it is not yet
// reachable from any handle the caller holds). The runtime registers a
// thread-local collector that finds the current agent and collects from its
// roots; outside an agent window (bootstrap) the collector is a no-op.
type StressCollector = Box<dyn Fn(GcAny)>;
thread_local! {
    static STRESS: Cell<bool> = const { Cell::new(false) };
    static STRESS_COLLECTOR: RefCell<Option<StressCollector>> =
        const { RefCell::new(None) };
    static COLLECTING: Cell<bool> = const { Cell::new(false) };
    /// A traced `RefCell` was mutably borrowed during marking: the sweep
    /// would free boxes the mark could not see, so it is aborted (retain
    /// everything) instead.
    static ABORT_SWEEP: Cell<bool> = const { Cell::new(false) };
    /// GC-3: the ephemeron edges (WeakMap key→value, WeakSet element→itself)
    /// registered while tracing the weak tables. A value is only reachable
    /// while its key is reachable from other roots, so the edges are
    /// deferred: the mark phase promotes a value once its key is marked,
    /// iterating to a fixpoint. Valid only during one collection.
    static EPHEMERONS: RefCell<Vec<(GcAny, GcAny)>> = const { RefCell::new(Vec::new()) };
    /// A2: the remembered set — the addresses of old boxes that may hold a
    /// young reference. Deduplicated by the box's `FLAG_REMEMBERED` bit, so
    /// the barrier needs no hash lookup; drained by every collection.
    static REMEMBERED: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    /// A2: whether the collector verifies the write barrier's exactness on
    /// every collection (the barrier is only testable while the young bits
    /// are still meaningful, so it runs before the sweep). On by default in
    /// debug builds; the release sweep enables it explicitly.
    static VERIFY_BARRIER: Cell<bool> = const { Cell::new(cfg!(debug_assertions)) };
    /// A3: whether a minor collection verifies its own mark with a full
    /// precise mark before sweeping (`--gc-verify`). Off by default: the
    /// verification is O(live) per minor. On, a barrier or generation-rule gap
    /// fails loudly instead of freeing a reachable box.
    static VERIFY_MINOR: Cell<bool> = const { Cell::new(false) };
    /// A5.1: the current collection interval's stamp. A type that bounds its
    /// dirty slots stamps a mark with it, so a mark from a previous interval
    /// reads as stale — and stale correctly means "no dirty slots", because
    /// every box that survived the last collection had its young children
    /// promoted. Bumped at the end of every collection, including the aborts.
    static COLLECTION_GEN: Cell<u32> = const { Cell::new(1) };
}

/// Record that a traced `RefCell` was borrowed during marking; the sweep
/// must be aborted.
pub fn note_aborted_trace() {
    ABORT_SWEEP.with(|abort| abort.set(true));
}

/// GC-3: register an ephemeron edge — `value` is reachable only while
/// `key` is reachable from other roots (WeakMap: the value lives while its
/// key does; WeakSet: the element is its own key). The collector marks the
/// value once the key is marked, so a weak table never retains its key.
pub fn note_ephemeron(key: GcAny, value: GcAny) {
    EPHEMERONS.with(|slot| slot.borrow_mut().push((key, value)));
}

/// Enable or disable the A2 write-barrier verifier (see [`VERIFY_BARRIER`]).
/// The release sweep turns it on by flag; debug builds default it on.
pub fn set_verify_barrier(enabled: bool) {
    VERIFY_BARRIER.with(|flag| flag.set(enabled));
}

/// A2: whether the barrier verifier is enabled.
pub fn verify_barrier_enabled() -> bool {
    VERIFY_BARRIER.with(|flag| flag.get())
}

/// A3 `--gc-verify`: enable the minor collection's self-check (see
/// [`VERIFY_MINOR`]).
pub fn set_verify_minor(enabled: bool) {
    VERIFY_MINOR.with(|flag| flag.set(enabled));
}

/// A3: whether the minor collection's self-check is enabled.
pub fn verify_minor_enabled() -> bool {
    VERIFY_MINOR.with(|flag| flag.get())
}

/// The write barrier (A2). Call after storing `value` into a field of the box
/// that owns `target`.
///
/// It records the target when a young value is stored into an *old* box — the
/// only kind of edge a young collection cannot recover on its own — so the
/// collector can trace a handful of old boxes instead of the whole old
/// generation. It imposes no ordering: a store into a young box (the common
/// constructor case) exits on one load.
///
/// Precondition (the caller's rooting discipline): `target` is a reference
/// into a live box's payload, never a stack temporary.
pub fn write_barrier<T: Trace>(target: &T, value: crate::value::Value) {
    let Some(child) = crate::value::Value::encoded_box_address(value.bits()) else {
        return;
    };
    // `encoded_box_address` returns the box of a live heap value.
    remember_if_young(barrier_target(target), GcAny(child as *mut GcHeader), None);
}

/// [`write_barrier`] for a store into a dense array's element `index` (A5.1).
/// It does everything [`write_barrier`] does, and on the old-target path it also
/// reports the slot to the box's type, so a minor can trace just the elements
/// written since the last collection instead of the whole buffer. The slot
/// report rides the branch that already has the box old — a young target cannot
/// hold an old->young edge and exits before either.
pub fn write_barrier_element<T: Trace>(target: &T, value: crate::value::Value, index: u64) {
    let Some(child) = crate::value::Value::encoded_box_address(value.bits()) else {
        return;
    };
    // A slot beyond `u32` would saturate the low-water mark upward and lose
    // elements below it, so it degrades to 0 (trace the whole buffer) — sound,
    // and unreachable for a dense buffer whose indices are bounded by its own
    // length.
    let index = if index > u32::MAX as u64 {
        0
    } else {
        index as u32
    };
    remember_if_young(
        barrier_target(target),
        GcAny(child as *mut GcHeader),
        Some(index),
    );
}

/// [`write_barrier`] for a store of a GC handle — a `Gc<T>`/`Handle<T>` field
/// (`Map::transitions`, `JsObject::prototype`, a captured environment) rather
/// than a `Value`.
pub fn write_barrier_handle<T: Trace, U: Trace>(target: &T, child: Gc<U>) {
    remember_if_young(barrier_target(target), child.as_any(), None);
}

/// The box that owns `target`. The caller's rooting discipline guarantees
/// `target` is a live box payload.
#[inline]
fn barrier_target<T: Trace>(target: &T) -> GcAny {
    let addr = (target as *const T as usize).wrapping_sub(std::mem::offset_of!(GcBox<T>, data));
    // A misused call site (a stack temporary) would compute a bogus box and
    // read unrelated flags. Catch it in the test suite; the check is two
    // compares and only in debug builds.
    #[cfg(debug_assertions)]
    debug_assert!(
        with_heap(|heap| heap.owns(addr)),
        "the write barrier target must reference a live box payload"
    );
    // `addr` is a live box's header, per the precondition.
    GcAny(addr as *mut GcHeader)
}

/// A5.1: whether `value` is a heap reference whose box is young. Read by the
/// dirty-bound assertion (verify mode only).
pub fn value_box_is_young(value: crate::value::Value) -> bool {
    let Some(addr) = crate::value::Value::encoded_box_address(value.bits()) else {
        return false;
    };
    // SAFETY: `encoded_box_address` yields the box of a live heap value.
    unsafe { (*(addr as *mut GcHeader)).is_young() }
}

/// Record `target` when `child` is young and `target` is old, and report the
/// dirty slot (A5.1) on that same path when the caller supplies one.
#[inline]
fn remember_if_young(target: GcAny, child: GcAny, dirty: Option<u32>) {
    // SAFETY: both are live boxes (the rooting discipline).
    unsafe {
        let header = target.0;
        // Fast path: a young target cannot hold an old->young edge, and the
        // hot shape is a constructor populating `this` — one load and a
        // not-taken branch.
        if (*header).is_young() {
            return;
        }
        if !(*child.0).is_young() {
            return;
        }
        // A5.1: the low-water mark is noted here rather than at the call site so
        // the hot young-target path above never pays for it, and so a site
        // cannot report a slot without also recording the edge (the two are what
        // a minor needs together).
        if let Some(index) = dirty {
            ((*header).vtable.note_dirty)((*header).data_ptr(target.0), index);
        }
        // The header bit is the dedup: an already-remembered box needs no
        // second entry, and no collector state is touched here.
        if !(*header).is_remembered() {
            (*header).set_remembered(true);
            REMEMBERED.with(|slot| slot.borrow_mut().push(target.addr()));
        }
    }
}

/// A2: whether any box is currently remembered (nothing is, immediately after
/// a collection — the whole heap is old then).
pub fn remembered_count() -> usize {
    REMEMBERED.with(|slot| slot.borrow().len())
}

/// Enable the per-allocation stress collector. `collect` receives the fresh
/// box of every allocation so it can be rooted through the collection.
pub fn enable_stress_collector(collect: StressCollector) {
    STRESS.with(|stress| stress.set(true));
    STRESS_COLLECTOR.with(|slot| *slot.borrow_mut() = Some(collect));
}

/// Disable the per-allocation stress collector.
pub fn disable_stress_collector() {
    STRESS.with(|stress| stress.set(false));
    STRESS_COLLECTOR.with(|slot| *slot.borrow_mut() = None);
}

/// Run the stress collector after an allocation (GC-2). No-op when stress is
/// off, when no collector is registered (outside an agent window), or when a
/// collection is already running (the mark/sweep must not re-enter itself).
fn maybe_stress_collect(fresh: GcAny) {
    if !STRESS.with(|stress| stress.get()) || COLLECTING.with(|collecting| collecting.get()) {
        return;
    }
    COLLECTING.with(|collecting| collecting.set(true));
    STRESS_COLLECTOR.with(|slot| {
        if let Some(collect) = &*slot.borrow() {
            collect(fresh);
        }
    });
    COLLECTING.with(|collecting| collecting.set(false));
}

/// Drain a young mark work list (A3): mark each young box once and push its
/// young children. An old box in the list is skipped untouched — its young
/// children are the remembered set's job, and the minor never sets an old mark
/// bit.
fn drain_young_work(work: &mut Vec<GcAny>) {
    while let Some(any) = work.pop() {
        // SAFETY: every `GcAny` in `work` is a live box (a root, a box the
        // conservative scan or remembered set contributed, or a traced child);
        // the mark bit breaks cycles.
        unsafe {
            if (*any.0).is_marked() || !(*any.0).is_young() {
                continue;
            }
            (*any.0).set_marked(true);
            any.trace(&mut |child| {
                if (*child.0).is_young() {
                    work.push(child);
                }
            });
        }
    }
}

/// Conservatively scan heap regions (the opaque job-closure boxes) for box
/// addresses and encoded `Value` payloads, visiting every live box found
/// (GC-2). A `Box<dyn FnOnce>` job closure holds its captured `Value`s as
/// raw bytes that no precise `Trace` can reach; scanning the closure's
/// allocation roots those captures. Imprecise by design: it may retain
/// garbage, never frees a live box.
pub fn scan_regions(regions: &[(*const u8, usize)], visit: &mut dyn FnMut(GcAny)) {
    HEAP.with(|heap| {
        let heap = heap.borrow();
        let mut by_addr: AddrMap = AddrMap::default();
        heap.for_each_live(|header| {
            by_addr.insert(header as usize, GcAny(header));
        });
        for (base, len) in regions {
            let mut addr = *base as usize;
            let end = addr + *len;
            while addr + std::mem::size_of::<usize>() <= end {
                // SAFETY: the region is the live allocation of a queued or
                // running job closure; reads are unaligned.
                let word = unsafe { std::ptr::read_unaligned::<usize>(addr as *const usize) };
                if let Some(&any) = by_addr.get(&word) {
                    visit(any);
                } else if let Some(box_addr) = crate::value::Value::encoded_box_address(word as u64)
                    && let Some(&any) = by_addr.get(&box_addr)
                {
                    visit(any);
                }
                addr += std::mem::size_of::<usize>();
            }
        }
    });
}

/// Run `f` with the current thread's heap.
pub fn with_heap<R>(f: impl FnOnce(&Heap) -> R) -> R {
    HEAP.with(|heap| f(&heap.borrow()))
}

/// Run `f` with mutable access to the current thread's heap.
pub fn with_heap_mut<R>(f: impl FnOnce(&mut Heap) -> R) -> R {
    HEAP.with(|heap| f(&mut heap.borrow_mut()))
}

impl Default for Heap {
    fn default() -> Self {
        Heap::new()
    }
}

impl Heap {
    pub const fn new() -> Heap {
        Heap {
            chunks: Vec::new(),
            order: Vec::new(),
            free: [const { Vec::new() }; FREE_CLASSES],
            live_boxes: 0,
            young: Vec::new(),
            arena_low: usize::MAX,
            arena_high: 0,
            stack_words: Cell::new(0),
        }
    }

    /// Number of live boxes (the growth trigger and the leak harness).
    pub fn live_count(&self) -> usize {
        self.live_boxes
    }

    /// Number of boxes allocated since the last collection (A1's cohort).
    pub fn young_count(&self) -> usize {
        self.young.len()
    }

    /// The number of arena chunks (A5.1): under size-classed churn the
    /// free list reuses swept slots, so the arena must not grow with the
    /// allocation count.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// The total number of swept slots waiting for reuse (A5.1).
    pub fn free_count(&self) -> usize {
        self.free.iter().map(|slots| slots.len()).sum()
    }

    /// Allocate `size` bytes aligned to `align` in the arena, reusing a
    /// swept slot of the same rounded size when one is free, else bumping
    /// into the current chunk (growing the arena by a chunk when full).
    /// `size` must already be rounded to [`ARENA_GRANULARITY`] (the `Gc`
    /// constructors round it once).
    fn alloc(&mut self, size: usize, align: usize) -> *mut u8 {
        debug_assert_eq!(size % ARENA_GRANULARITY, 0, "callers round the size");
        // The arena walk recovers slots by stepping `size` from the chunk
        // start, so an allocation may not insert padding. Every `GcBox<T>`
        // alignment is at most 8, so `align_up` is a no-op on the granular-
        // aligned bump pointer.
        debug_assert!(
            align <= ARENA_GRANULARITY,
            "the arena walk assumes granularity-bounded alignment"
        );
        // Size-classed free-list reuse first: a swept box of this exact
        // rounded size is the common hot shape — direct-indexed, no hash.
        if let Some(index) = free_index(size)
            && let Some(slot) = self.free[index].pop()
        {
            return slot as *mut u8;
        }
        let bump = self.chunks.last().map_or(0, |chunk| chunk.bump);
        let aligned = align_up(bump, align);
        if let Some(chunk) = self.chunks.last_mut()
            && aligned + size <= chunk.end
        {
            chunk.bump = aligned + size;
            return aligned as *mut u8;
        }
        self.push_chunk();
        let chunk = self.chunks.last_mut().expect("a chunk was just pushed");
        let aligned = align_up(chunk.bump, align);
        chunk.bump = aligned + size;
        aligned as *mut u8
    }

    /// Append a fresh arena chunk. The buffer is uninitialized (no zeroing
    /// cost per chunk); the allocator writes every slot before use.
    fn push_chunk(&mut self) {
        let mut buffer: Vec<std::mem::MaybeUninit<u8>> = Vec::with_capacity(ARENA_CHUNK_SIZE);
        // SAFETY: the capacity is exactly the chunk size and the memory is
        // never read before a box is written into it.
        unsafe {
            buffer.set_len(ARENA_CHUNK_SIZE);
        }
        let data = buffer.into_boxed_slice();
        // SAFETY: `MaybeUninit<u8>` and `u8` have identical layout, so the
        // boxed slice erases the `MaybeUninit` wrapper without changing the
        // allocation's deallocation layout.
        let data: Box<[u8]> = unsafe { Box::from_raw(Box::into_raw(data) as *mut [u8]) };
        let raw = data.as_ptr() as usize;
        let start = align_up(raw, ARENA_GRANULARITY);
        let end = raw + ARENA_CHUNK_SIZE;
        let index = self.chunks.len();
        self.chunks.push(ArenaChunk {
            data,
            start,
            bump: start,
            end,
        });
        self.arena_low = self.arena_low.min(start);
        self.arena_high = self.arena_high.max(end);
        // Keep `order` sorted by chunk base so the arena walk yields an
        // ascending box list (the stack scan binary-searches it). Chunk
        // pushes are rare (one per MiB), so the insert is not hot.
        let position = self
            .order
            .binary_search_by_key(&start, |&i| self.chunks[i].start)
            .unwrap_or_else(|position| position);
        self.order.insert(position, index);
    }

    /// Record a freshly allocated (young) box, which the collector cannot see
    /// until it is rooted.
    fn note_alloc(&mut self, addr: usize) {
        self.live_boxes += 1;
        self.young.push(addr);
    }

    /// Visit every live box in ascending address order. The arena walk
    /// replaces the `live` registry (A0): every slot — live or swept — keeps
    /// a valid `size`, so the walk steps exactly and the live bit filters.
    fn for_each_live(&self, mut f: impl FnMut(*mut GcHeader)) {
        for &index in &self.order {
            let chunk = &self.chunks[index];
            let mut addr = chunk.start;
            while addr + size_of::<GcHeader>() <= chunk.bump {
                // SAFETY: `addr` is a slot start inside this chunk's
                // allocated range (`[start, bump)`), and every slot's header
                // was written at allocation and survives the sweep.
                let header = addr as *mut GcHeader;
                let size = unsafe { (*header).size as usize };
                debug_assert!(size >= size_of::<GcHeader>());
                if unsafe { (*header).is_live() } {
                    f(header);
                }
                addr += size;
            }
        }
    }

    /// The live boxes in ascending address order, for the conservative stack
    /// scan's binary search. Built by the arena walk (already sorted), so no
    /// per-collection sort is needed.
    fn live_sorted(&self) -> Vec<GcAny> {
        let mut live = Vec::with_capacity(self.live_boxes);
        self.for_each_live(|header| live.push(GcAny(header)));
        live
    }

    /// The address range spanned by the arena's allocated slots. A pre-filter
    /// for the stack scan: most stack words are not box addresses, and two
    /// compares beat a binary search per word. It may include dead slots and
    /// inter-chunk gaps, so it never excludes a live box.
    fn live_range(&self) -> (usize, usize) {
        if self.arena_low == usize::MAX {
            (0, 0)
        } else {
            (self.arena_low, self.arena_high)
        }
    }

    /// Whether `addr` could be a box address in this heap (a superset: it may
    /// also contain free slots and inter-chunk gaps). The write barrier's
    /// debug-only misuse check.
    #[cfg(debug_assertions)]
    fn owns(&self, addr: usize) -> bool {
        (self.arena_low..self.arena_high).contains(&addr)
    }

    /// Mark-sweep from `roots`. Reachable boxes are kept (and unmarked for the
    /// next cycle); everything else is dropped and its memory freed. Marking
    /// is iterative (an explicit worklist), so a deeply nested object graph
    /// (a long rope, a deep prototype chain) cannot overflow the native
    /// stack.
    pub fn collect(&mut self, roots: &[GcAny]) -> Vec<usize> {
        let work = roots.to_vec();
        self.reset_trace_start();
        let trace = self.trace_start();
        let swept = self.collect_from_work(work, roots, false, &mut |_, _| {});
        self.trace_end("major", trace, swept.len());
        swept
    }
}

/// GC-4: the collector's compaction hook — `dead` lists the addresses of
/// the boxes that would be swept (still allocated, so their values are
/// readable), and `retain` marks a box the hook needs to keep alive through
/// the sweep (a captured FinalizationRegistry heldValue).
pub type CompactHook<'a> = dyn FnMut(&[usize], &mut dyn FnMut(GcAny)) + 'a;

/// The current thread's committed stack region `[low, high)`, or `None`
/// when the platform cannot provide it (the collector then relies on the
/// precise roots alone). The conservative native-stack scan marks every live
/// box whose address appears as a stack word, so Rust locals and closure
/// captures holding `Gc<T>` or `Value` survive collection.
#[cfg(windows)]
fn stack_bounds() -> Option<(usize, usize)> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        // Windows 8+; works for the main thread and worker threads alike.
        fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize) -> i32;
    }
    let mut low = 0usize;
    let mut high = 0usize;
    // SAFETY: kernel32 writes the two locals; both pointers are valid for
    // writes and the function always initializes them before returning.
    let ok = unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    if ok != 0 && low != 0 && high > low {
        Some((low, high))
    } else {
        None
    }
}

/// Linux: the current thread's stack top. `pthread_getattr_np`/`pthread_attr_getstack`
/// report the exact usable range of the calling thread (guard excluded) — the same
/// source the Boehm GC uses. Parsing `/proc/self/maps` alone is not reliable here:
/// adjacent per-thread stacks can be covered by one mapping (and split by nothing
/// the scan can see), so the region containing the stack pointer may extend past
/// this thread's stack into a neighbour's guard page, faulting the scan.
#[cfg(target_os = "linux")]
fn stack_bounds() -> Option<(usize, usize)> {
    // A named local's address is a genuine stack location; taking the address
    // of the literal (`&0usize`) would be const-promoted to the binary's
    // read-only data, so any mapping lookup would match the wrong region.
    let probe = 0usize;
    let sp = &probe as *const usize as usize;
    let high = match (pthread_stack_top(), maps_stack_top(sp)) {
        (Some(a), Some(m)) => Some(a.min(m)),
        (a, m) => a.or(m),
    }?;
    (high > sp).then_some((sp, high))
}

#[cfg(target_os = "linux")]
fn pthread_stack_top() -> Option<usize> {
    // SAFETY: `pthread_getattr_np` initializes `attr` for the current thread;
    // after it succeeds the attribute is valid for `pthread_attr_getstack` and
    // must be released with `pthread_attr_destroy`.
    unsafe {
        let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
        if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
            return None;
        }
        let mut base: *mut libc::c_void = std::ptr::null_mut();
        let mut size = 0usize;
        let got = libc::pthread_attr_getstack(attr.as_ptr(), &mut base, &mut size);
        let destroyed = libc::pthread_attr_destroy(attr.as_mut_ptr());
        if got == 0 && destroyed == 0 && !base.is_null() && size > 0 {
            Some(base as usize + size)
        } else {
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn maps_stack_top(sp: usize) -> Option<usize> {
    // The end of the readable mapping containing `sp` (the guard page is a
    // separate `---p` mapping, excluded by the read-permission check). Only a
    // clamp on the pthread bounds: it is the *smaller* endpoint that wins, so
    // a merged multi-stack mapping can never widen the scan, only narrow it.
    let text = std::fs::read_to_string("/proc/self/maps").ok()?;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(range), Some(perms)) = (fields.next(), fields.next()) else {
            continue;
        };
        if !perms.contains('r') {
            continue;
        }
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (
            usize::from_str_radix(start, 16),
            usize::from_str_radix(end, 16),
        ) else {
            continue;
        };
        if start <= sp && sp < end {
            return Some(end);
        }
    }
    None
}

/// wasm32: the shadow stack lives in the module's linear memory (rustc's
/// wasm layout), and wasm has no OS stack API to query. Scan from the
/// current frame to the end of the current memory. The region above the
/// real stack top is the dlmalloc heap and static data that share linear
/// memory; the conservative scan only ever retains (never falsely frees),
/// so the wider region is safe — it just widens retention. See the GC-7
/// note in .notes/gc-plan.md: capturing the module-initial stack pointer
/// would bound the scan to the actual stack.
#[cfg(target_arch = "wasm32")]
fn stack_bounds() -> Option<(usize, usize)> {
    let probe = 0usize;
    let sp = &probe as *const usize as usize;
    let end = core::arch::wasm32::memory_size::<0>().saturating_mul(65536);
    (end > sp).then_some((sp, end))
}

/// Platforms without a stack-bounds source: no conservative scan.
#[cfg(not(any(windows, target_os = "linux", target_arch = "wasm32")))]
fn stack_bounds() -> Option<(usize, usize)> {
    None
}

impl Heap {
    /// Mark-sweep with a conservative native-stack scan: every live box
    /// whose address appears on the current thread's stack (a raw `Gc<T>`
    /// local or an encoded `Value` payload) is marked before the precise
    /// `roots` are traced, then the sweep frees everything unmarked. The
    /// scan is the safety net for Rust-held handles that no precise root
    /// can see; it may retain garbage, never free a reachable box.
    pub fn collect_with_stack(&mut self, roots: &[GcAny]) -> Vec<usize> {
        self.collect_with_stack_compacting(roots, false, &mut |_, _| {})
    }

    /// [`Heap::collect_with_stack`] with a GC-4 compaction hook: `compact`
    /// runs between the mark and the sweep with the addresses of the boxes
    /// that would be swept (still allocated, so the weak tables can capture
    /// values into cleanup jobs) and a `retain` closure that marks boxes the
    /// hook needs to keep alive (a captured heldValue). The retained boxes
    /// are traced and the ephemeron fixpoint re-runs before the sweep.
    ///
    /// `precise` requests a second, scan-free mark (GC-4): the compaction's
    /// dead set comes from it, so a stale stack word cannot keep a WeakRef
    /// or FinalizationRegistry target alive. The sweep still uses the
    /// conservative mark — the scan remains the safety net for Rust-held
    /// handles, and clearing a weak entry never frees a box.
    pub fn collect_with_stack_compacting(
        &mut self,
        roots: &[GcAny],
        precise: bool,
        compact: &mut CompactHook<'_>,
    ) -> Vec<usize> {
        // The scan starts at a local's address (this frame) and runs to the
        // stack top, covering every caller frame that may hold a handle.
        let stack_bottom_marker = 0usize;
        let sp = &stack_bottom_marker as *const usize as usize;
        let mut work: Vec<GcAny> = roots.to_vec();
        self.reset_trace_start();
        let trace = self.trace_start();
        if let Some((_low, high)) = stack_bounds()
            && high > sp
        {
            // A0: the arena walk yields the live boxes already sorted by
            // address (the stack scan binary-searches that list), so no
            // per-collection sort is needed.
            let live_sorted = self.live_sorted();
            self.scan_stack(sp, high, &live_sorted, &mut work);
        }
        let swept = self.collect_from_work(work, roots, precise, compact);
        self.trace_end("major", trace, swept.len());
        swept
    }

    /// A3: a minor (young-generation) collection.
    ///
    /// Only the boxes allocated since the last collection are at risk, so only
    /// they are swept; the old generation's mark bits are never touched. The
    /// mark is seeded from the precise roots, the conservative stack scan
    /// restricted to young boxes, and the remembered set — the old boxes the
    /// write barrier recorded. A traced child that is old is not followed: its
    /// young children are exactly what the remembered set holds. That rule is
    /// what keeps the cost O(young + remembered + the roots' fan-out) rather
    /// than O(live).
    ///
    /// An old box counts as marked for the ephemeron fixpoint (a minor never
    /// sweeps it, so its weakly held value must survive). The boxes the mark did
    /// not reach drive the weak compaction through `compact`, exactly like the
    /// major's dead set — except that the set holds young boxes only, so a
    /// `is_dead` test can never misfire on an old key or target.
    pub fn collect_minor_with_stack(
        &mut self,
        roots: &[GcAny],
        compact: &mut CompactHook<'_>,
    ) -> Vec<usize> {
        self.collect_minor_inner(roots, true, compact)
    }

    /// [`Heap::collect_minor_with_stack`] without the conservative stack scan:
    /// the deterministic entry point ([`Heap::collect`]'s counterpart for A3).
    pub fn collect_minor(&mut self, roots: &[GcAny]) -> Vec<usize> {
        self.collect_minor_inner(roots, false, &mut |_, _| {})
    }

    fn collect_minor_inner(
        &mut self,
        roots: &[GcAny],
        stack_scan: bool,
        compact: &mut CompactHook<'_>,
    ) -> Vec<usize> {
        // The A2 invariant is exactly what a minor's correctness rests on, so
        // it is audited here too (the verifier is O(live) and therefore only on
        // in debug builds and `--gc-stress`).
        self.verify_barrier();
        self.reset_trace_start();
        let trace = self.trace_start();
        let mut work: Vec<GcAny> = Vec::new();
        // A precise root that is young is marked; one that is old is only
        // stepped through to reach its direct young children.
        for &any in roots {
            // SAFETY: every root is a live box.
            unsafe { self.seed_minor(any, &mut work) };
        }
        // The conservative stack scan, restricted to young boxes: an old box on
        // the stack contributes nothing on its own, and the young cohort is far
        // shorter than the arena walk the major's scan binary-searches.
        let probe = 0usize;
        let sp = &probe as *const usize as usize;
        if stack_scan
            && let Some((_low, high)) = stack_bounds()
            && high > sp
        {
            let mut young_sorted: Vec<GcAny> = self
                .young
                .iter()
                .map(|&addr| GcAny(addr as *mut GcHeader))
                .collect();
            young_sorted.sort_unstable_by_key(|any| any.addr());
            self.scan_stack(sp, high, &young_sorted, &mut work);
        }
        // The remembered set: the old boxes a young store reached. They are old,
        // so they are stepped through but never marked — a young box's mark bit
        // is the only kind this collection sets.
        let remembered: Vec<usize> = REMEMBERED.with(|slot| slot.borrow().clone());
        for addr in remembered {
            // SAFETY: the remembered set holds live old boxes (the barrier
            // recorded them, and every collection drains the set before one of
            // its boxes can be freed).
            unsafe { self.seed_minor(GcAny(addr as *mut GcHeader), &mut work) };
        }
        self.mark_young(&mut work);
        // A traced `RefCell` was mutably borrowed mid-mark: the mark is
        // incomplete, so nothing is swept. The young cohort and the remembered
        // set both stay exactly as they were, and the next collection retries.
        if ABORT_SWEEP.with(|abort| abort.replace(false)) {
            for &addr in &self.young {
                // SAFETY: the young list holds live boxes.
                unsafe { (*(addr as *mut GcHeader)).set_marked(false) };
            }
            EPHEMERONS.with(|slot| slot.borrow_mut().clear());
            bump_collection_generation();
            compact(&[], &mut |_| {});
            return Vec::new();
        }
        // The young boxes the mark did not reach, collected while they are
        // still allocated: the compaction hook needs the would-be-swept
        // addresses to drop the weak entries that name them.
        let mut dead = self.dead_young();
        dead.sort_unstable();
        let mut retained: Vec<GcAny> = Vec::new();
        compact(&dead, &mut |any| retained.push(any));
        if !retained.is_empty() {
            // A retained held value is reachable only from a pending cleanup
            // job, so the sweep must keep it. A young one is marked; an old one
            // needs nothing (it is not swept, and any later young store into it
            // was barriered). An old box is never marked here: the minor sweep
            // clears only young mark bits, so a stray old mark bit would survive
            // into the next major and hide that box's children.
            for any in retained {
                // SAFETY: a retained box is live (the sweep has not run).
                unsafe { self.seed_minor(any, &mut work) };
            }
            self.mark_young(&mut work);
            dead = self.dead_young();
            dead.sort_unstable();
        }
        // `--gc-verify`: a full precise mark from the same roots, asserting that
        // nothing about to be swept is reachable. The ephemerons are still
        // registered (they are cleared only below).
        if verify_minor_enabled()
            && let Some((container, child)) = self.minor_reachable_offender(roots, &dead)
        {
            EPHEMERONS.with(|slot| slot.borrow_mut().clear());
            self.drain_remembered();
            panic!(
                "minor collection would sweep a reachable box: {container} -> {child} \
                 ({} dead young box(es) of {} young)",
                dead.len(),
                self.young.len()
            );
        }
        EPHEMERONS.with(|slot| slot.borrow_mut().clear());
        // The caches that own GC handles the collector does not trace must be
        // pruned while the mark bits are final: the major does it in
        // `collect_from_work`, and a minor needs the young-only counterpart
        // (an old cache entry is alive by definition here).
        crate::map::drop_unmarked_young_empty_maps();
        let mut swept = Vec::new();
        for &addr in &self.young {
            let header = addr as *mut GcHeader;
            // SAFETY: the young list holds live boxes throughout the sweep; the
            // walk clears the live bit before the payload is dropped. An
            // unmarked young box has no live handle (the rooting discipline),
            // so dropping its payload cannot dangle one.
            unsafe {
                if (*header).is_marked() {
                    // A survivor: promote it in place.
                    (*header).set_marked(false);
                    (*header).set_young(false);
                } else {
                    (*header).set_live(false);
                    (*header).set_marked(false);
                    (*header).set_young(false);
                    let size = (*header).size as usize;
                    ((*header).vtable.drop)((*header).data_ptr(header));
                    if let Some(class) = free_index(size) {
                        self.free[class].push(addr);
                    }
                    swept.push(addr);
                    self.live_boxes -= 1;
                }
            }
        }
        // Every young box is now promoted or dead, so every recorded old->young
        // edge has become old->old and the set is stale.
        self.young.clear();
        self.drain_remembered();
        self.debug_assert_young_drained();
        self.verify_no_marks_left();
        bump_collection_generation();
        self.trace_end("minor", trace, swept.len());
        swept
    }

    /// Push `any` when it is young; otherwise step through its direct children
    /// and push the young ones, without marking `any`. An old box is never
    /// marked by a minor collection (see [`Heap::collect_minor_with_stack`]).
    ///
    /// SAFETY: `any` must be a live box.
    unsafe fn seed_minor(&self, any: GcAny, work: &mut Vec<GcAny>) {
        // SAFETY: the caller guarantees `any` is a live box.
        unsafe {
            if (*any.0).is_young() {
                work.push(any);
            } else {
                // A5.1: an old box's *young* children can only have come from a
                // store the barrier saw (a store while it was young would have
                // been traced when it was traced as a young box, and its child
                // promoted with it), so the type may bound the scan to the slots
                // written since the last collection.
                any.trace_dirty(&mut |child| {
                    if (*child.0).is_young() {
                        work.push(child);
                    }
                });
            }
        }
    }

    /// Mark the young boxes in `work` and follow their young children, then
    /// resolve the ephemeron edges to a fixpoint. An old box counts as marked:
    /// a minor never sweeps it, so a value it weakly holds must survive.
    fn mark_young(&self, work: &mut Vec<GcAny>) {
        drain_young_work(work);
        loop {
            let mut promoted = false;
            let edges = EPHEMERONS.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
            for (key, value) in edges {
                // SAFETY: the ephemerons were registered while tracing live
                // boxes, so both ends are live for the duration of the mark.
                let promote = unsafe {
                    let key_reachable = (*key.0).is_marked() || !(*key.0).is_young();
                    let value_reachable = (*value.0).is_marked() || !(*value.0).is_young();
                    key_reachable && !value_reachable
                };
                if promote {
                    // Push unmarked so the drain marks *and traces* it.
                    work.push(value);
                    promoted = true;
                }
                EPHEMERONS.with(|slot| slot.borrow_mut().push((key, value)));
            }
            if !promoted {
                break;
            }
            drain_young_work(work);
        }
    }

    /// The young boxes the current minor mark did not reach.
    fn dead_young(&self) -> Vec<usize> {
        let mut dead = Vec::new();
        for &addr in &self.young {
            // SAFETY: the young list holds live boxes.
            if !unsafe { (*(addr as *mut GcHeader)).is_marked() } {
                dead.push(addr);
            }
        }
        dead
    }

    /// A3 `--gc-verify`: a full precise mark (young and old alike) from
    /// `roots`, reporting the first box in `dead` that the mark reaches. `dead`
    /// must have been built while those boxes are still allocated, so their
    /// type names are readable.
    ///
    /// This is the net that turns a barrier or generation-rule gap into a loud
    /// failure instead of a use-after-free at the next access.
    fn minor_reachable_offender(
        &self,
        roots: &[GcAny],
        dead: &[usize],
    ) -> Option<(&'static str, &'static str)> {
        if dead.is_empty() {
            return None;
        }
        // Tracing a mutably-borrowed `RefCell` calls `note_aborted_trace`, so
        // the verifier's own traversal must not abort the real collection.
        let saved_abort = ABORT_SWEEP.with(|abort| abort.get());
        let mut marked = AddrSet::default();
        let mut parent_of: AddrMap = AddrMap::default();
        let mut work: Vec<GcAny> = roots.to_vec();
        loop {
            while let Some(any) = work.pop() {
                let addr = any.addr();
                if !marked.insert(addr) {
                    continue;
                }
                // SAFETY: `any` is a live box (a root or a traced child); the
                // mark set breaks cycles.
                unsafe {
                    any.trace(&mut |child| {
                        parent_of.entry(child.addr()).or_insert(any);
                        work.push(child);
                    });
                }
            }
            let mut promoted = false;
            let edges = EPHEMERONS.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
            for (key, value) in edges {
                if marked.contains(&key.addr()) && !marked.contains(&value.addr()) {
                    work.push(value);
                    promoted = true;
                }
                EPHEMERONS.with(|slot| slot.borrow_mut().push((key, value)));
            }
            if !promoted {
                break;
            }
        }
        // An incomplete verifier mark would report false positives, so it is
        // skipped: the aborted trace already forced the real sweep's abort.
        let aborted = ABORT_SWEEP.with(|abort| abort.replace(saved_abort));
        if aborted && !saved_abort {
            return None;
        }
        for &addr in dead {
            if marked.contains(&addr) {
                // SAFETY: `addr` is a live box (the sweep has not run yet).
                let child = unsafe { ((*(addr as *mut GcHeader)).vtable.name)() };
                let container = parent_of
                    .get(&addr)
                    // SAFETY: the recorded parent is a live box.
                    .map(|any| unsafe { ((*any.0).vtable.name)() })
                    .unwrap_or("<root>");
                return Some((container, child));
            }
        }
        None
    }

    /// The mark phase shared by [`Heap::collect`] and
    /// [`Heap::collect_with_stack`]: drain `work` (the roots plus any boxes
    /// the conservative stack scan found) iteratively, run the GC-4
    /// compaction hook, then sweep. Returns the addresses of the swept
    /// boxes.
    ///
    /// GC-4: `precise_roots` are the precise roots only — the stack scan's
    /// findings in `work` are imprecise, since a stale word in a popped
    /// frame can retain a box the heap no longer reaches. When `precise` is
    /// requested and the scan found boxes, a second scan-free mark runs
    /// first and the compaction's dead set comes from it; the sweep still
    /// uses the conservative mark.
    fn collect_from_work(
        &mut self,
        mut work: Vec<GcAny>,
        precise_roots: &[GcAny],
        precise: bool,
        compact: &mut CompactHook<'_>,
    ) -> Vec<usize> {
        // A2: the barrier's exactness is only testable while the young bits
        // still describe the pre-collection heap, so it runs first.
        self.verify_barrier();
        // GC-4: precise dead set for the weak tables. Run before the
        // conservative mark so the compaction decides liveness from true
        // heap reachability (plus the ephemeron fixpoint), never from stale
        // stack words. The ephemeron edges registered while tracing the
        // roots flow through to the conservative pass below.
        let precise_marked: Option<AddrSet> = if precise && work.len() > precise_roots.len() {
            let mut marked = AddrSet::default();
            let mut pwork: Vec<GcAny> = precise_roots.to_vec();
            while let Some(any) = pwork.pop() {
                let addr = any.addr();
                if !marked.insert(addr) {
                    continue;
                }
                // SAFETY: `any` is a live box (a precise root); the mark set
                // breaks cycles.
                unsafe {
                    any.trace(&mut |child| pwork.push(child));
                }
            }
            loop {
                let mut promoted = false;
                let edges = EPHEMERONS.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
                for (key, value) in edges {
                    let key_marked = marked.contains(&key.addr());
                    let value_marked = marked.contains(&value.addr());
                    if key_marked && !value_marked {
                        pwork.push(value);
                        promoted = true;
                    }
                    EPHEMERONS.with(|slot| slot.borrow_mut().push((key, value)));
                }
                if !promoted {
                    break;
                }
                while let Some(any) = pwork.pop() {
                    let addr = any.addr();
                    if !marked.insert(addr) {
                        continue;
                    }
                    // SAFETY: as above; the promoted value is a live box.
                    unsafe {
                        any.trace(&mut |child| pwork.push(child));
                    }
                }
            }
            Some(marked)
        } else {
            None
        };
        // SAFETY: every `GcAny` in `work` is a live box (a root from a live
        // `Gc<T>`'s `Trace` impl, or a box the scan looked up in the arena);
        // the mark bit breaks cycles.
        while let Some(any) = work.pop() {
            unsafe {
                if any.is_marked() {
                    continue;
                }
                any.set_marked(true);
                any.trace(&mut |child| work.push(child));
            }
        }
        // GC-3 ephemeron fixpoint: a weak-table value is reachable only
        // while its key is reachable from other roots. Each pass promotes
        // the values whose keys are now marked (their trace can register
        // further edges — e.g. a WeakMap value that is itself a WeakMap key
        // — so the passes repeat until nothing new is promoted).
        loop {
            let mut promoted = false;
            let edges = EPHEMERONS.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
            for (key, value) in edges {
                let key_marked = key.is_marked();
                let value_marked = value.is_marked();
                if key_marked && !value_marked {
                    // Push unmarked: the drain below marks *and traces* the
                    // promoted value (a pre-mark would make the drain skip
                    // it, leaving its children unmarked and sweepable).
                    work.push(value);
                    promoted = true;
                }
                EPHEMERONS.with(|slot| slot.borrow_mut().push((key, value)));
            }
            if !promoted {
                break;
            }
            while let Some(any) = work.pop() {
                unsafe {
                    if any.is_marked() {
                        continue;
                    }
                    any.set_marked(true);
                    any.trace(&mut |child| work.push(child));
                }
            }
        }
        EPHEMERONS.with(|slot| slot.borrow_mut().clear());
        // A traced `RefCell` was mutably borrowed mid-mark (per-allocation
        // `--gc-stress`): the mark is incomplete, so retain everything —
        // imprecise but safe. The next collection retries.
        let aborted = ABORT_SWEEP.with(|abort| abort.replace(false));
        if aborted {
            // Reset the marks and promote every live box (the sweep is
            // skipped, so nothing else is reclassified), keeping the young
            // list drained. Retaining everything is imprecise but safe; the
            // next collection retries.
            self.for_each_live(|header| {
                // SAFETY: `header` belongs to a live box.
                unsafe {
                    (*header).set_marked(false);
                    (*header).set_young(false);
                }
            });
            self.young.clear();
            self.debug_assert_young_drained();
            self.drain_remembered();
            bump_collection_generation();
            compact(&[], &mut |_| {});
            return Vec::new();
        }
        // GC-4: the compaction hook sees the would-be-swept addresses while
        // the boxes are still allocated. Retained boxes are marked and
        // re-traced, and the ephemeron fixpoint re-runs (a retained
        // heldValue may itself be a weak key).
        let mut dead: Vec<usize> = Vec::new();
        if let Some(precise_marked) = precise_marked.as_ref() {
            self.for_each_live(|header| {
                let addr = header as usize;
                if !precise_marked.contains(&addr) {
                    dead.push(addr);
                }
            });
        } else {
            self.for_each_live(|header| {
                // SAFETY: `header` belongs to a live box.
                if !unsafe { (*header).is_marked() } {
                    dead.push(header as usize);
                }
            });
        }
        dead.sort_unstable();
        let mut retained: Vec<GcAny> = Vec::new();
        compact(&dead, &mut |any| retained.push(any));
        while let Some(any) = retained.pop() {
            let mut work = vec![any];
            while let Some(any) = work.pop() {
                unsafe {
                    if any.is_marked() {
                        continue;
                    }
                    any.set_marked(true);
                    any.trace(&mut |child| work.push(child));
                }
            }
            // Re-run the fixpoint for edges reachable from the retained box.
            loop {
                let mut promoted = false;
                let edges = EPHEMERONS.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
                for (key, value) in edges {
                    if key.is_marked() && !value.is_marked() {
                        // Push unmarked so the drain traces it (see the
                        // main fixpoint).
                        work.push(value);
                        promoted = true;
                    }
                    EPHEMERONS.with(|slot| slot.borrow_mut().push((key, value)));
                }
                if !promoted {
                    break;
                }
                while let Some(any) = work.pop() {
                    unsafe {
                        if any.is_marked() {
                            continue;
                        }
                        any.set_marked(true);
                        any.trace(&mut |child| work.push(child));
                    }
                }
            }
            EPHEMERONS.with(|slot| slot.borrow_mut().clear());
        }
        // GC-6: the sweep frees the unmarked boxes below. Prune the caches
        // that own GC handles the collector does not trace (the canonical
        // empty maps) while the mark bits are still final, so a box this
        // sweep drops is never handed out again.
        crate::map::drop_unmarked_empty_maps();
        // A0: sweep by walking the arena. Every slot — live or swept — keeps
        // its rounded size, so the walk steps exactly; the live bit filters.
        let mut swept = Vec::new();
        for index in 0..self.chunks.len() {
            let (start, bump) = (self.chunks[index].start, self.chunks[index].bump);
            let mut addr = start;
            while addr + size_of::<GcHeader>() <= bump {
                // SAFETY: `addr` is a slot start inside this chunk's allocated
                // range; its header is valid. An unmarked live box has no live
                // handle (the rooting discipline), so dropping its payload
                // cannot dangle one.
                let header = addr as *mut GcHeader;
                unsafe {
                    let size = (*header).size as usize;
                    debug_assert!(size >= size_of::<GcHeader>());
                    if (*header).is_live() {
                        if (*header).is_marked() {
                            // A survivor: clear the mark and promote it in
                            // place (A1) — the young bit is what a minor
                            // collection partitions on.
                            (*header).set_marked(false);
                            (*header).set_young(false);
                        } else {
                            // Dead: clear the live bit (the walk then skips the
                            // slot) before the payload is dropped.
                            (*header).set_live(false);
                            ((*header).vtable.drop)((*header).data_ptr(header));
                            // Reclaim the slot on the size-classed free list
                            // (reused by a later `Gc::new` of the same size),
                            // never freed to the allocator. Sizes beyond the
                            // classes bump forever (rare; the arena grows).
                            if let Some(class) = free_index(size) {
                                self.free[class].push(addr);
                            }
                            swept.push(addr);
                            self.live_boxes -= 1;
                        }
                    }
                    addr += size;
                }
            }
        }
        // A1: the collection reclassified every survivor, so the cohort is
        // empty until the next allocation.
        self.young.clear();
        self.debug_assert_young_drained();
        self.drain_remembered();
        self.verify_no_marks_left();
        bump_collection_generation();
        swept
    }

    /// A2: clear the remembered set. A full collection reclassifies the whole
    /// heap — every surviving box is promoted to old and the young cohort is
    /// drained — so no old->young edge survives it and the recorded addresses
    /// are stale. The header bits are cleared too, so a slot later reused for
    /// a young box never carries a stale entry (`Gc::new` rewrites the whole
    /// flags word regardless).
    fn drain_remembered(&self) {
        REMEMBERED.with(|slot| {
            let mut list = slot.borrow_mut();
            for &addr in list.iter() {
                // SAFETY: the address came from a live box; boxes are only
                // freed inside a collection and the header outlives the
                // payload as a free-list slot, so the flags write is in
                // bounds.
                unsafe { (*(addr as *mut GcHeader)).set_remembered(false) };
            }
            list.clear();
        });
    }

    /// A2: verify the write barrier's completeness — every live *old* box that
    /// holds a young reference must be in the remembered set. Runs before the
    /// sweep, while the young bits still describe the pre-collection heap.
    ///
    /// Only a miss is fatal (A3 would sweep a reachable box). A remembered box
    /// with no young child is imprecise — it costs one extra trace, never
    /// correctness — and is counted, not asserted: unwinding a store that
    /// overwrote a young value with a primitive would need a delete barrier.
    fn verify_barrier(&self) {
        if !verify_barrier_enabled() {
            return;
        }
        // Tracing a mutably-borrowed `RefCell` calls `note_aborted_trace`, so
        // the verifier's own traversal must not abort the real collection.
        let saved_abort = ABORT_SWEEP.with(|abort| abort.get());
        let mut misses = 0usize;
        let mut imprecise = 0usize;
        let mut offenders: Vec<(&'static str, &'static str)> = Vec::new();
        self.for_each_live(|header| {
            // SAFETY: `header` is a live box for the duration of the walk.
            unsafe {
                if (*header).is_young() {
                    return;
                }
                let mut has_young_child = false;
                let mut young_child = "";
                GcAny(header).trace(&mut |child| {
                    if (*child.0).is_young() {
                        has_young_child = true;
                        if young_child.is_empty() {
                            young_child = ((*child.0).vtable.name)();
                        }
                    }
                });
                match ((*header).is_remembered(), has_young_child) {
                    (false, true) => {
                        misses += 1;
                        if offenders.len() < 4 {
                            offenders.push((((*header).vtable.name)(), young_child));
                        }
                    }
                    (true, false) => imprecise += 1,
                    _ => {}
                }
            }
        });
        ABORT_SWEEP.with(|abort| abort.set(saved_abort));
        assert!(
            misses == 0,
            "write-barrier miss: {misses} old box(es) hold a young reference the barrier never \
             recorded ({imprecise} imprecise entries); offenders: {offenders:?}"
        );
    }

    /// A3: after a collection, no live box may carry a mark bit. A minor leaves
    /// the old generation's mark bits alone, so a stray one would make the next
    /// major skip that box — its children would then be swept while reachable,
    /// which is invisible to the minor's own reachability check (that reads no
    /// flags). Runs only when a verifier is enabled (it walks the arena).
    fn verify_no_marks_left(&self) {
        if !verify_minor_enabled() && !verify_barrier_enabled() {
            return;
        }
        let mut stray: Option<(&'static str, bool)> = None;
        self.for_each_live(|header| {
            // SAFETY: `header` belongs to a live box.
            unsafe {
                if (*header).is_marked() && stray.is_none() {
                    stray = Some((((*header).vtable.name)(), (*header).is_young()));
                }
            }
        });
        assert!(
            stray.is_none(),
            "a collection left a mark bit set: {stray:?} (type, is_young)"
        );
    }

    /// A1 invariant: after a collection no live box is young and the cohort
    /// list is empty. Debug-only (the check walks the arena); release relies
    /// on the sweep's per-survivor promotion.
    fn debug_assert_young_drained(&self) {
        #[cfg(debug_assertions)]
        {
            debug_assert!(
                self.young.is_empty(),
                "the young cohort must be drained by every collection"
            );
            self.for_each_live(|header| {
                // SAFETY: `header` belongs to a live box.
                debug_assert!(
                    !unsafe { (*header).is_young() },
                    "a box that survived a collection must be promoted"
                );
            });
        }
    }

    /// Scan every word in the current thread's live stack region
    /// `[sp, high)` and push boxes whose address appears there onto `work`.
    /// `live_sorted` is the arena's live boxes in ascending address order
    /// (the walk yields it, A0); a membership test is a binary search, so a
    /// coincidental stack word can only be marked when it is a real box
    /// (imprecise, never unsafe).
    fn scan_stack(&self, sp: usize, high: usize, live_sorted: &[GcAny], work: &mut Vec<GcAny>) {
        // GC-5: most stack words are not box addresses — skip the search for
        // words outside the arena's address range.
        let (live_low, live_high) = self.live_range();
        let find = |addr: usize| {
            live_sorted
                .binary_search_by_key(&addr, |any| any.addr())
                .ok()
                .map(|index| live_sorted[index])
        };
        // A box address can appear in many scanned words (every stored
        // reference to it); push each box once so the work list stays bounded
        // by the live set no matter how wide the scanned region is.
        let mut seen: AddrSet = AddrSet::default();
        let mut addr = sp;
        let mut words = 0usize;
        while addr < high {
            // SAFETY: `[sp, high)` is the current thread's committed stack
            // (platform stack_bounds guarantees readability); reads are
            // unaligned so the exact frame layout does not matter.
            let word = unsafe { std::ptr::read_unaligned::<usize>(addr as *const usize) };
            if (live_low..=live_high).contains(&word)
                && let Some(any) = find(word)
            {
                // The scan only pushes boxes already in the live set.
                if seen.insert(any.addr()) {
                    work.push(any);
                }
            } else if let Some(box_addr) = crate::value::Value::encoded_box_address(word as u64)
                && (live_low..=live_high).contains(&box_addr)
                && let Some(any) = find(box_addr)
            {
                // A box decoded from a tagged Value; the scan only pushes
                // boxes already live.
                if seen.insert(box_addr) {
                    work.push(any);
                }
            }
            addr += std::mem::size_of::<usize>();
            words += 1;
        }
        self.stack_words.set(words);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Node {
        next: GcCell<Option<Gc<Node>>>,
    }

    impl Trace for Node {
        fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
            self.next.trace(visit);
        }
    }

    #[test]
    fn handles_deref_to_the_payload() {
        let gc = Gc::new(Node::default());
        gc.next.borrow_mut().replace(Gc::new(Node::default()));
        assert!(gc.next.borrow().is_some());
    }

    #[test]
    fn ptr_eq_compares_box_identity() {
        let a = Gc::new(Node::default());
        let b = Gc::new(Node::default());
        assert!(Gc::ptr_eq(a, a));
        assert!(!Gc::ptr_eq(a, b));
        let copied = a;
        assert!(Gc::ptr_eq(copied, a));
    }

    #[test]
    fn unreachable_boxes_are_swept() {
        let start = with_heap(|heap| heap.live_count());
        // Create a self-cycle with no external handle.
        {
            let a = Gc::new(Node::default());
            let b = Gc::new(Node::default());
            a.next.borrow_mut().replace(b);
            b.next.borrow_mut().replace(a);
        }
        assert_eq!(with_heap(|heap| heap.live_count()), start + 2);
        with_heap_mut(|heap| heap.collect(&[]));
        assert_eq!(
            with_heap(|heap| heap.live_count()),
            start,
            "cycle is unreachable and swept"
        );
    }

    #[test]
    fn reachable_boxes_survive_collection() {
        let a = Gc::new(Node::default());
        let b = Gc::new(Node::default());
        a.next.borrow_mut().replace(b);
        b.next.borrow_mut().replace(a);
        let start = with_heap(|heap| heap.live_count());
        with_heap_mut(|heap| heap.collect(&[a.as_any()]));
        assert_eq!(
            with_heap(|heap| heap.live_count()),
            start,
            "rooted cycle is kept"
        );
        // Both nodes remain usable through the root.
        assert!(a.next.borrow().is_some());
        assert!(b.next.borrow().is_some());
    }

    #[test]
    fn sweeping_frees_acyclic_graphs_and_reuses_nothing_twice() {
        let start = with_heap(|heap| heap.live_count());
        for _ in 0..100 {
            let head = Gc::new(Node::default());
            head.next.borrow_mut().replace(Gc::new(Node::default()));
        }
        with_heap_mut(|heap| heap.collect(&[]));
        assert_eq!(with_heap(|heap| heap.live_count()), start);
    }

    #[test]
    fn arena_reuses_swept_slots_under_churn() {
        // A5.1: churning same-size boxes with a collection between batches
        // must reuse the swept slots — the arena chunk count stays bounded
        // instead of growing with the allocation count.
        for batch in 0..4 {
            for _ in 0..256 {
                let node = Gc::new(Node::default());
                node.next.borrow_mut().replace(Gc::new(Node::default()));
            }
            with_heap_mut(|heap| heap.collect(&[]));
            assert!(
                with_heap(|heap| heap.chunk_count()) <= 2,
                "batch {batch}: arena grew to {} chunks",
                with_heap(|heap| heap.chunk_count())
            );
            assert!(
                with_heap(|heap| heap.free_count()) >= 128,
                "batch {batch}: free list has {} slots",
                with_heap(|heap| heap.free_count())
            );
        }
    }

    #[test]
    fn repeated_collection_is_idempotent() {
        let a = Gc::new(Node::default());
        let start = with_heap(|heap| heap.live_count());
        with_heap_mut(|heap| heap.collect(&[a.as_any()]));
        with_heap_mut(|heap| heap.collect(&[a.as_any()]));
        with_heap_mut(|heap| heap.collect(&[a.as_any()]));
        assert_eq!(with_heap(|heap| heap.live_count()), start);
        let child = Gc::new(Node::default());
        write_barrier_handle(&*a, child);
        a.next.borrow_mut().replace(child);
        with_heap_mut(|heap| heap.collect(&[]));
        assert_eq!(
            with_heap(|heap| heap.live_count()),
            start - 1,
            "only the root survives"
        );
        let _ = std::hint::black_box(a);
    }

    #[test]
    fn barrier_records_an_old_to_young_edge() {
        set_verify_barrier(true);
        let a = Gc::new(Node::default());
        // Promote `a` with it as the only root.
        with_heap_mut(|heap| heap.collect(&[a.as_any()]));
        // A young child stored into the now-old `a` must be recorded...
        let child = Gc::new(Node::default());
        write_barrier_handle(&*a, child);
        assert_eq!(remembered_count(), 1);
        // ...which satisfies the collection's own verifier, and the set is
        // drained afterwards (a full collection leaves no young boxes).
        with_heap_mut(|heap| heap.collect(&[a.as_any()]));
        assert_eq!(remembered_count(), 0);
        // SAFETY: `a` survived the collection.
        unsafe { assert!(!(*a.as_any().0).is_remembered()) };
        let _ = std::hint::black_box((a, child));
    }

    #[test]
    #[should_panic(expected = "write-barrier miss")]
    fn a_missing_barrier_is_detected() {
        set_verify_barrier(true);
        let a = Gc::new(Node::default());
        with_heap_mut(|heap| heap.collect(&[a.as_any()]));
        // A young child stored WITHOUT the barrier: the collector must catch
        // it rather than sweep the child at the next collection.
        let child = Gc::new(Node::default());
        a.next.borrow_mut().replace(child);
        with_heap_mut(|heap| heap.collect(&[a.as_any()]));
    }

    #[test]
    fn boxes_are_born_young_and_promoted_in_place() {
        let a = Gc::new(Node::default());
        let doomed = Gc::new(Node::default());
        // SAFETY: both handles are live boxes.
        unsafe {
            assert!((*a.as_any().0).is_young(), "a fresh box is young");
            assert!((*doomed.as_any().0).is_young(), "a fresh box is young");
        }
        assert!(with_heap(|heap| heap.young_count()) >= 2);
        // The collection promotes the rooted survivor in place, sweeps the
        // unrooted box, and drains the cohort.
        with_heap_mut(|heap| heap.collect(&[a.as_any()]));
        // SAFETY: `a` survived the collection.
        unsafe {
            assert!(
                !(*a.as_any().0).is_young(),
                "a survivor is promoted in place"
            );
        }
        assert_eq!(with_heap(|heap| heap.young_count()), 0);
        // A fresh allocation starts the next cohort.
        let fresh = Gc::new(Node::default());
        // SAFETY: `fresh` is live.
        unsafe { assert!((*fresh.as_any().0).is_young()) };
        assert_eq!(with_heap(|heap| heap.young_count()), 1);
        let _ = std::hint::black_box((a, fresh));
    }

    #[test]
    fn minor_keeps_a_young_box_reached_through_the_remembered_set() {
        let old = Gc::new(Node::default());
        // Promote `old` (it is the only root).
        with_heap_mut(|heap| heap.collect(&[old.as_any()]));
        // A young child stored into the now-old box: the barrier records the
        // edge, which is the only thing that can reach the child (the minor is
        // given no precise root).
        let child = Gc::new(Node::default());
        write_barrier_handle(&*old, child);
        old.next.borrow_mut().replace(child);
        let swept = with_heap_mut(|heap| heap.collect_minor(&[]));
        assert!(swept.is_empty(), "the recorded edge kept the child alive");
        assert_eq!(with_heap(|heap| heap.young_count()), 0);
        assert!(old.next.borrow().is_some());
        let _ = std::hint::black_box((old, child, swept));
    }

    #[test]
    fn minor_sweeps_an_unreachable_young_box() {
        let live = Gc::new(Node::default());
        with_heap_mut(|heap| heap.collect(&[live.as_any()]));
        let doomed = Gc::new(Node::default());
        let swept = with_heap_mut(|heap| heap.collect_minor(&[live.as_any()]));
        assert_eq!(
            swept,
            vec![doomed.as_any().addr()],
            "the unrooted young box is swept"
        );
        // The old generation is untouched.
        assert_eq!(with_heap(|heap| heap.young_count()), 0);
        let _ = std::hint::black_box((live, doomed));
    }

    #[test]
    fn a_major_after_a_minor_still_traces_the_old_generation() {
        let old = Gc::new(Node::default());
        with_heap_mut(|heap| heap.collect(&[old.as_any()]));
        let child = Gc::new(Node::default());
        write_barrier_handle(&*old, child);
        old.next.borrow_mut().replace(child);
        // The minor promotes `child`. It must leave no old mark bit behind: the
        // minor sweep clears only young marks, so a stray one would make the
        // next major skip that box's children.
        let swept = with_heap_mut(|heap| heap.collect_minor(&[]));
        assert!(swept.is_empty());
        let grandchild = Gc::new(Node::default());
        write_barrier_handle(&*child, grandchild);
        child.next.borrow_mut().replace(grandchild);
        let swept = with_heap_mut(|heap| heap.collect(&[old.as_any()]));
        assert!(
            !swept.contains(&grandchild.as_any().addr()),
            "the major must reach through the minor-promoted child"
        );
        let _ = std::hint::black_box((old, child, grandchild, swept));
    }

    #[test]
    #[should_panic(expected = "minor collection would sweep a reachable box")]
    fn minor_verify_detects_a_missing_barrier() {
        // The A2 verifier catches the missing barrier first, and this test is
        // about the minor's own net, so it is switched off.
        set_verify_barrier(false);
        set_verify_minor(true);
        let root = Gc::new(Node::default());
        with_heap_mut(|heap| heap.collect(&[root.as_any()]));
        // An old->old edge: no barrier needed, and the major below reaches it.
        let mid = Gc::new(Node::default());
        root.next.borrow_mut().replace(mid);
        with_heap_mut(|heap| heap.collect(&[root.as_any()]));
        // The bug: an old->young store with no barrier, two hops from the root,
        // so the minor's own mark cannot reach it (it stops at the old `mid`)
        // while a full mark can.
        let leaf = Gc::new(Node::default());
        mid.next.borrow_mut().replace(leaf);
        with_heap_mut(|heap| heap.collect_minor(&[root.as_any()]));
    }

    #[test]
    fn a_minor_traces_only_an_old_arrays_dirty_slots() {
        use crate::object::ArraySlots;
        // A5.1: the low-water mark bounds what a minor scans. Fill the buffer so
        // a full trace has plenty to visit, promote it, then append one young
        // element and check both that the scan is bounded and that the young
        // element survives it.
        let undef = crate::value::Value::Undefined;
        let mut elements = vec![undef; 8];
        let old: Vec<Gc<crate::object::JsObject>> = (0..6)
            .map(|_| crate::object::JsObject::ordinary_object_create(None))
            .collect();
        for (index, object) in old.iter().enumerate() {
            elements[index] = crate::value::Value::Object(*object);
        }
        let slots = Gc::new(ArraySlots::new(elements, 8.0));
        // Promote the array and its six elements.
        with_heap_mut(|heap| heap.collect(&[slots.as_any()]));
        let child = crate::object::JsObject::ordinary_object_create(None);
        slots.elements_mut()[6] = crate::value::Value::Object(child);
        write_barrier_element(&*slots, crate::value::Value::Object(child), 6);
        // The bound is real: the dirty trace visits slot 6 onwards (slot 7 is a
        // hole, so only the child), while the full trace visits every element.
        let mut dirty_visits = 0;
        // SAFETY: `slots` is a live old box.
        unsafe { slots.as_any().trace_dirty(&mut |_| dirty_visits += 1) };
        assert_eq!(
            dirty_visits, 1,
            "only the slots at or after the mark are visited"
        );
        let mut full_visits = 0;
        // SAFETY: as above.
        unsafe { slots.as_any().trace(&mut |_| full_visits += 1) };
        assert_eq!(
            full_visits, 7,
            "the full trace visits every live element (six old + the young child)"
        );
        // And the bound is sound: the young element is reachable only through
        // the old array, so the minor must mark (and promote) it.
        let swept = with_heap_mut(|heap| heap.collect_minor(&[]));
        assert!(swept.is_empty(), "the young element survived: {swept:?}");
        assert!(child.as_any().is_live());
    }

    #[test]
    fn arena_walk_multi_chunk_varied_sizes() {
        #[derive(Default)]
        struct Wide {
            next: GcCell<Option<Gc<Node>>>,
            /// Padding only: it widens the box so the loop spans several
            /// arena chunks.
            #[allow(dead_code)]
            pad: [u64; 32],
        }

        impl Trace for Wide {
            fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
                self.next.trace(visit);
            }
        }

        for round in 0..8 {
            let mut keep: Vec<Gc<Wide>> = Vec::new();
            for i in 0..8000 {
                let wide = Gc::new(Wide::default());
                wide.next.borrow_mut().replace(Gc::new(Node::default()));
                if i % 7 == 0 {
                    keep.push(wide);
                }
            }
            let roots: Vec<GcAny> = keep.iter().map(|wide| wide.as_any()).collect();
            with_heap_mut(|heap| heap.collect_with_stack(&roots));
            for wide in &keep {
                assert!(
                    wide.next.borrow().is_some(),
                    "round {round}: dropped a root"
                );
            }
        }
    }

    #[test]
    fn stack_scan_roots_local_gc_handles() {
        let start = with_heap(|heap| heap.live_count());
        let a = Gc::new(Node::default());
        let b = Gc::new(Node::default());
        a.next.borrow_mut().replace(b);
        b.next.borrow_mut().replace(a);
        // The cycle is reachable only through the stack locals; the
        // conservative scan must keep it alive with no explicit roots.
        // (black_box takes the addresses, forcing both locals to stack
        // slots the scan can see.)
        std::hint::black_box(&a);
        std::hint::black_box(&b);
        with_heap_mut(|heap| heap.collect_with_stack(&[]));
        assert!(a.next.borrow().is_some());
        assert!(b.next.borrow().is_some());
        assert_eq!(with_heap(|heap| heap.live_count()), start + 2);
        let _ = std::hint::black_box((a, b));
    }

    #[test]
    fn stack_scan_roots_encoded_value_payloads() {
        use crate::Handle;
        use crate::string::JsString;
        use crate::value::{Value, ValueKind};
        let start = with_heap(|heap| heap.live_count());
        let value = Value::String(Handle::new(JsString::from_utf8("stack-scanned")));
        // `value` is the only reference; taking its address spills it to a
        // stack slot the scan can see (a `Value` is a NaN-boxed word).
        std::hint::black_box(&value);
        with_heap_mut(|heap| heap.collect_with_stack(&[]));
        assert!(matches!(value.kind(), ValueKind::String(_)));
        assert_eq!(with_heap(|heap| heap.live_count()), start + 1);
        let _ = std::hint::black_box(&value);
    }
}
