//! The GC heap (docs/gc-plan.md, GC-1): `Gc<T>` handles into a thread-local
//! mark-sweep heap.
//!
//! Modeled on the `gc` crate: each traced object lives in a `GcBox<T>` that
//! is individually heap-allocated, and the thread-local heap keeps a registry
//! of every live box for the mark phase. `Gc<T>` derefs directly to its box's
//! payload (no arena lookup), so existing `handle.field` call sites survive
//! the migration from `Handle<T> = Rc<T>`.
//!
//! Soundness invariant: **every live `Gc<T>` must be reachable from the roots
//! passed to [`Heap::collect`]** (or from a conservative stack scan, which the
//! arena refinement in GC-1 adds). A `Gc<T>` that is unmarked at sweep time
//! is dropped while the handle still exists — a use-after-free. The
//! `--gc-stress` mode (collect on every allocation) is the test net for this.
//!
//! Slice 1 is precise-roots only: callers pass the roots explicitly. The
//! runtime migration (roots from agent tables, VM stacks, job closures) and
//! the conservative native-stack scan land in the following GC-1 slices.

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
}

/// An erased GC reference: the box pointer for any `Gc<T>`. Produced by
/// `Gc<T>`'s `Trace` impl and consumed as a [`Heap::collect`] root.
#[derive(Clone, Copy)]
pub struct GcAny(*mut GcBox<dyn Trace>);

impl GcAny {
    /// The box base address (the identity used by the conservative stack
    /// scan and the weak-table compaction after a collection).
    pub fn addr(self) -> usize {
        self.0 as *const u8 as usize
    }
}

/// A cell in the GC heap. `T` is unsized only through `dyn Trace`; for a
/// typed `Gc<T>` the box is sized. The header holds the mark bit and the
/// box's rounded arena size (A5.1: the arena walk steps by it and the
/// size-classed free list keys by it); `data` is the payload the handle
/// derefs to.
#[repr(C)]
struct GcBox<T: ?Sized + Trace> {
    mark: Cell<bool>,
    /// The box's total arena footprint (header + data, rounded to
    /// [`ARENA_GRANULARITY`]), written once at allocation.
    size: u32,
    data: T,
}

/// The offset of a boxed value's data within its `GcBox` (the box header —
/// `mark` + `size` — precedes the value): the compiled member-cell probe
/// adds this to the NaN-boxing payload's box base to reach the `JsObject`.
/// The header fields are fixed, so the offset is a stable ABI constant.
pub const GCBOX_DATA_OFFSET: usize = std::mem::offset_of!(GcBox<crate::Value>, data);

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
        GcAny(self.ptr.as_ptr() as *mut GcBox<dyn Trace>)
    }

    /// Pointer identity, replacing `Rc::ptr_eq` for the migration.
    pub fn ptr_eq(self, other: Gc<T>) -> bool {
        self.ptr == other.ptr
    }
}

impl<T: Trace> Gc<T> {
    /// Allocate a new box in the bump arena and register it. A5.1: replaces
    /// the per-box `Box::new` malloc with a bump + size-classed free-list
    /// reuse inside one heap borrow.
    pub fn new(value: T) -> Gc<T> {
        let size = round_up(size_of::<GcBox<T>>(), ARENA_GRANULARITY);
        let gc = with_heap_mut(|heap| {
            let raw = heap.alloc(size, align_of::<GcBox<T>>()) as *mut GcBox<T>;
            // SAFETY: `raw` is a fresh arena slot (bumped or reused) of at
            // least `size` bytes; writing the header + payload initializes
            // the box before any handle can see it.
            unsafe {
                raw.write(GcBox {
                    mark: Cell::new(false),
                    size: size as u32,
                    data: value,
                });
            }
            heap.register(raw as *mut GcBox<dyn Trace>);
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
                boxed.mark = Cell::new(false);
                boxed.size = size as u32;
                init(std::ptr::addr_of_mut!(boxed.data));
            }
            heap.register(raw as *mut GcBox<dyn Trace>);
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
    /// Reclaimed slots by rounded size class (see [`FREE_CLASSES`]): swept
    /// (dead) boxes are reused by `Gc::new` before the bump advances. Boxes
    /// are never freed individually — the arena keeps the memory, and slots
    /// cycle through the free list.
    free: [Vec<*mut GcBox<dyn Trace>>; FREE_CLASSES],
    live: Vec<*mut GcBox<dyn Trace>>,
    /// Address range of the registered boxes, refreshed by the sweep (GC-5):
    /// the stack scan pre-filter skips words outside it — most stack words
    /// are not box addresses, and two compares are far cheaper than a
    /// HashMap lookup per word.
    live_min: usize,
    live_max: usize,
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
/// build and the precise dead set.
#[derive(Default)]
struct FxHasher(u64);

impl std::hash::Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 =
                (self.0.rotate_left(5) ^ u64::from(byte)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

type AddrMap = std::collections::HashMap<
    usize,
    *mut GcBox<dyn Trace>,
    std::hash::BuildHasherDefault<FxHasher>,
>;

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
    /// pure overhead), disabling mid-loop collections until the next script
    /// boundary; a collection that reclaimed garbage keeps them eager.
    static BUDGET_BACKOFF: Cell<u64> = const { Cell::new(1) };
}

/// The allocation budget that paces safe-point collections (GC-5): after
/// this many allocations since the last check, the runtime runs its real
/// collection trigger (`Agent::maybe_collect`, which still gates on the
/// live count). 1024 keeps the heap bounded at a few thousand garbage
/// boxes in a hot allocation loop while the back-edge check itself stays a
/// single compare.
const ALLOC_BUDGET: u64 = 1073741824; // TEMP-EXPERIMENT

/// GC-5: the cheap safe-point check for loop back-edges. Returns true when
/// enough allocations have happened since the last check (the caller then
/// runs its collection trigger); the counter is reset either way. An empty
/// sweep multiplies the budget by [`BUDGET_BACKOFF`] (see
/// [`note_collection`]). `#[inline]` so the back-edge check is a TLS read +
/// compare (cross-crate calls are not inlined otherwise).
#[inline]
pub fn allocation_budget_exceeded() -> bool {
    // Fast path: below the base budget the backoff cannot matter — one TLS
    // read and out (the machinery rows never allocate, so this is the hot
    // shape: counter is 0).
    if ALLOC_SINCE_COLLECT.with(|count| count.get()) < ALLOC_BUDGET {
        return false;
    }
    let budget = ALLOC_BUDGET.saturating_mul(BUDGET_BACKOFF.with(|backoff| backoff.get()));
    let exceeded = ALLOC_SINCE_COLLECT.with(|count| count.get()) >= budget;
    if exceeded {
        ALLOC_SINCE_COLLECT.with(|count| count.set(0));
    }
    exceeded
}

/// GC-5: reset the safe-point allocation budget (a script/job boundary —
/// the budget counts allocations since the last trigger or collection, so
/// it must not leak across scripts). Also re-enables mid-loop collections
/// that an empty sweep disabled.
pub fn reset_allocation_budget() {
    ALLOC_SINCE_COLLECT.with(|count| count.set(0));
    BUDGET_BACKOFF.with(|backoff| backoff.set(1));
}

/// GC-5: record a collection that ran (any path — a script/job boundary, a
/// safe point, or `--gc-stress`): the allocation budget restarts from zero
/// (so it counts allocations since the last collection, never drifting). A
/// collection that swept nothing is pure overhead for a growing live set (a
/// concat rope keeps every node reachable), so mid-loop collections are
/// disabled until the next script boundary; reclamation keeps them eager.
pub fn note_collection(swept: usize) {
    ALLOC_SINCE_COLLECT.with(|count| count.set(0));
    BUDGET_BACKOFF.with(|backoff| {
        if swept == 0 {
            backoff.set(u64::MAX);
        } else {
            backoff.set(1);
        }
    });
}

// GC-2 `--gc-stress`: collect after every allocation. The collector runs
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

/// Conservatively scan heap regions (the opaque job-closure boxes) for box
/// addresses and encoded `Value` payloads, visiting every live box found
/// (GC-2). A `Box<dyn FnOnce>` job closure holds its captured `Value`s as
/// raw bytes that no precise `Trace` can reach; scanning the closure's
/// allocation roots those captures. Imprecise by design: it may retain
/// garbage, never frees a live box.
pub fn scan_regions(regions: &[(*const u8, usize)], visit: &mut dyn FnMut(GcAny)) {
    HEAP.with(|heap| {
        let heap = heap.borrow();
        let by_addr: AddrMap = heap
            .live
            .iter()
            .map(|ptr| (*ptr as *const u8 as usize, *ptr))
            .collect();
        for (base, len) in regions {
            let mut addr = *base as usize;
            let end = addr + *len;
            while addr + std::mem::size_of::<usize>() <= end {
                // SAFETY: the region is the live allocation of a queued or
                // running job closure; reads are unaligned.
                let word = unsafe { std::ptr::read_unaligned::<usize>(addr as *const usize) };
                if let Some(&ptr) = by_addr.get(&word) {
                    visit(GcAny(ptr));
                } else if let Some(box_addr) = crate::value::Value::encoded_box_address(word as u64)
                    && let Some(&ptr) = by_addr.get(&box_addr)
                {
                    visit(GcAny(ptr));
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
            free: [const { Vec::new() }; FREE_CLASSES],
            live: Vec::new(),
            live_min: 0,
            live_max: 0,
        }
    }

    /// Number of live boxes (for the leak-detection harness).
    pub fn live_count(&self) -> usize {
        self.live.len()
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
        let base = align_up(raw, ARENA_GRANULARITY);
        let end = raw + ARENA_CHUNK_SIZE;
        self.chunks.push(ArenaChunk {
            data,
            bump: base,
            end,
        });
    }

    fn register(&mut self, boxed: *mut GcBox<dyn Trace>) {
        let addr = boxed as *const u8 as usize;
        if addr < self.live_min {
            self.live_min = addr;
        }
        if addr > self.live_max {
            self.live_max = addr;
        }
        self.live.push(boxed);
    }

    /// Mark-sweep from `roots`. Reachable boxes are kept (and unmarked for the
    /// next cycle); everything else is dropped and its memory freed. Marking
    /// is iterative (an explicit worklist), so a deeply nested object graph
    /// (a long rope, a deep prototype chain) cannot overflow the native
    /// stack.
    pub fn collect(&mut self, roots: &[GcAny]) -> Vec<usize> {
        let work = roots.to_vec();
        self.collect_from_work(work, roots, false, &mut |_, _| {})
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

/// Linux: the committed anonymous mapping containing the current stack
/// pointer (the main thread's `[stack]` entry and worker-thread stacks both
/// appear in `/proc/self/maps`; the guard page is a separate `---p` mapping,
/// excluded by the read-permission check).
#[cfg(target_os = "linux")]
fn stack_bounds() -> Option<(usize, usize)> {
    let sp = &0usize as *const usize as usize;
    let text = std::fs::read_to_string("/proc/self/maps").ok()?;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(range), Some(perms)) = (fields.next(), fields.next()) else {
            continue;
        };
        if !perms.contains('r') {
            // Guard pages (`---p`) fault on read; skip them.
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
            return Some((start, end));
        }
    }
    None
}

/// Platforms without a stack-bounds source: no conservative scan.
#[cfg(not(any(windows, target_os = "linux")))]
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
        // Sort the live list by box address once, so the conservative scan
        // resolves a stack word to its fat pointer by binary search — exact
        // (a random word can never be mistaken for a box) and far cheaper
        // than rebuilding an address HashMap every collection (A5.1b).
        self.live
            .sort_unstable_by_key(|ptr| *ptr as *const u8 as usize);
        // The scan starts at a local's address (this frame) and runs to the
        // stack top, covering every caller frame that may hold a handle.
        let stack_bottom_marker = 0usize;
        let sp = &stack_bottom_marker as *const usize as usize;
        let mut work: Vec<GcAny> = roots.to_vec();
        if let Some((_low, high)) = stack_bounds()
            && high > sp
        {
            self.scan_stack(sp, high, &self.live, &mut work);
        }
        self.collect_from_work(work, roots, precise, compact)
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
                // SAFETY: `any` is a registered box (a precise root);
                // the mark set breaks cycles.
                unsafe {
                    (*any.0).data.trace(&mut |child| pwork.push(child));
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
                    // SAFETY: as above; the promoted value is a
                    // registered box.
                    unsafe {
                        (*any.0).data.trace(&mut |child| pwork.push(child));
                    }
                }
            }
            Some(marked)
        } else {
            None
        };
        // SAFETY: every `GcAny` in `work` is a registered box (a root from a
        // live `Gc<T>`'s `Trace` impl, or an address the scan looked up in
        // the live set); the mark bit breaks cycles.
        while let Some(any) = work.pop() {
            unsafe {
                let ptr = any.0;
                if (*ptr).mark.get() {
                    continue;
                }
                (*ptr).mark.set(true);
                (*ptr).data.trace(&mut |child| work.push(child));
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
                let key_marked = unsafe { (*key.0).mark.get() };
                let value_marked = unsafe { (*value.0).mark.get() };
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
                    let ptr = any.0;
                    if (*ptr).mark.get() {
                        continue;
                    }
                    (*ptr).mark.set(true);
                    (*ptr).data.trace(&mut |child| work.push(child));
                }
            }
        }
        EPHEMERONS.with(|slot| slot.borrow_mut().clear());
        // A traced `RefCell` was mutably borrowed mid-mark (per-allocation
        // `--gc-stress`): the mark is incomplete, so retain everything —
        // imprecise but safe. The next collection retries.
        let aborted = ABORT_SWEEP.with(|abort| abort.replace(false));
        if aborted {
            for ptr in &self.live {
                // SAFETY: entries are registered boxes; resetting the mark
                // prepares for the next cycle.
                unsafe {
                    (**ptr).mark.set(false);
                }
            }
            compact(&[], &mut |_| {});
            return Vec::new();
        }
        // GC-4: the compaction hook sees the would-be-swept addresses while
        // the boxes are still allocated. Retained boxes are marked and
        // re-traced, and the ephemeron fixpoint re-runs (a retained
        // heldValue may itself be a weak key).
        let mut dead: Vec<usize> = Vec::new();
        if let Some(precise_marked) = &precise_marked {
            for ptr in &self.live {
                let addr = *ptr as *const u8 as usize;
                if !precise_marked.contains(&addr) {
                    dead.push(addr);
                }
            }
        } else {
            for ptr in &self.live {
                if !unsafe { (**ptr).mark.get() } {
                    dead.push(*ptr as *const u8 as usize);
                }
            }
        }
        dead.sort_unstable();
        let mut retained: Vec<GcAny> = Vec::new();
        compact(&dead, &mut |any| retained.push(any));
        while let Some(any) = retained.pop() {
            let mut work = vec![any];
            while let Some(any) = work.pop() {
                unsafe {
                    let ptr = any.0;
                    if (*ptr).mark.get() {
                        continue;
                    }
                    (*ptr).mark.set(true);
                    (*ptr).data.trace(&mut |child| work.push(child));
                }
            }
            // Re-run the fixpoint for edges reachable from the retained box.
            loop {
                let mut promoted = false;
                let edges = EPHEMERONS.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
                for (key, value) in edges {
                    if unsafe { (*key.0).mark.get() } && !unsafe { (*value.0).mark.get() } {
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
                        let ptr = any.0;
                        if (*ptr).mark.get() {
                            continue;
                        }
                        (*ptr).mark.set(true);
                        (*ptr).data.trace(&mut |child| work.push(child));
                    }
                }
            }
            EPHEMERONS.with(|slot| slot.borrow_mut().clear());
        }
        let mut keep = Vec::with_capacity(self.live.len());
        let mut swept = Vec::new();
        let mut new_min = usize::MAX;
        let mut new_max = 0usize;
        for ptr in self.live.drain(..) {
            // SAFETY: every entry was registered by `Gc::new` and is a valid
            // box; unmarked boxes have no live handles (the rooting
            // discipline), so dropping them cannot dangle a handle.
            unsafe {
                if (*ptr).mark.get() {
                    (*ptr).mark.set(false);
                    let addr = ptr as *const u8 as usize;
                    if addr < new_min {
                        new_min = addr;
                    }
                    if addr > new_max {
                        new_max = addr;
                    }
                    keep.push(ptr);
                } else {
                    swept.push(ptr as *const u8 as usize);
                    // A5.1: the arena owns the slot memory, but the dead
                    // box's payload must still be dropped (its Vecs,
                    // HashMaps, and Arc buffers are heap allocations of
                    // their own) before the slot is reused — otherwise
                    // every swept box leaks its internals.
                    std::ptr::drop_in_place(&mut (*ptr).data);
                    // Reclaim the slot on the size-classed free list
                    // (reused by a later `Gc::new` of the same size), never
                    // freed to the allocator. Sizes beyond the classes bump
                    // forever (rare; the arena grows for them).
                    if let Some(index) = free_index((*ptr).size as usize) {
                        self.free[index].push(ptr);
                    }
                }
            }
        }
        self.live = keep;
        self.live_min = if new_min == usize::MAX { 0 } else { new_min };
        self.live_max = new_max;
        swept
    }

    /// Scan every word in the current thread's live stack region
    /// `[sp, high)` and push boxes whose address appears there onto `work`.
    /// `live_sorted` is the `live` list sorted by box address (A5.1b); a
    /// membership test is a binary search, so a coincidental stack word can
    /// only be marked when it is a real box (imprecise, never unsafe).
    fn scan_stack(
        &self,
        sp: usize,
        high: usize,
        live_sorted: &[*mut GcBox<dyn Trace>],
        work: &mut Vec<GcAny>,
    ) {
        // GC-5: most stack words are not box addresses — skip the search
        // for words outside the live boxes' address range (tracked by
        // register and refreshed by the sweep).
        let live_low = self.live_min;
        let live_high = self.live_max;
        let find = |addr: usize| {
            live_sorted
                .binary_search_by_key(&addr, |ptr| *ptr as *const u8 as usize)
                .ok()
                .map(|index| live_sorted[index])
        };
        let mut addr = sp;
        while addr < high {
            // SAFETY: `[sp, high)` is the current thread's committed stack
            // (platform stack_bounds guarantees readability); reads are
            // unaligned so the exact frame layout does not matter.
            let word = unsafe { std::ptr::read_unaligned::<usize>(addr as *const usize) };
            if (live_low..=live_high).contains(&word)
                && let Some(ptr) = find(word)
            {
                // SAFETY: `ptr` is a registered, live box; the scan only
                // pushes boxes already in the live set.
                work.push(GcAny(ptr));
            } else if let Some(box_addr) = crate::value::Value::encoded_box_address(word as u64)
                && (live_low..=live_high).contains(&box_addr)
                && let Some(ptr) = find(box_addr)
            {
                // SAFETY: `ptr` is a registered, live box decoded from a
                // tagged Value; the scan only pushes boxes already live.
                work.push(GcAny(ptr));
            }
            addr += std::mem::size_of::<usize>();
        }
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
        a.next.borrow_mut().replace(Gc::new(Node::default()));
        with_heap_mut(|heap| heap.collect(&[]));
        assert_eq!(
            with_heap(|heap| heap.live_count()),
            start - 1,
            "only the root survives"
        );
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
