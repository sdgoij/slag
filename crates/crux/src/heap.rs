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

/// A cell in the GC heap. `T` is unsized only through `dyn Trace`; for a
/// typed `Gc<T>` the box is sized. The header holds the mark bit; `data` is
/// the payload the handle derefs to.
#[repr(C)]
struct GcBox<T: ?Sized + Trace> {
    mark: Cell<bool>,
    data: T,
}

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
    /// Allocate a new boxed value on the thread-local heap and register it.
    pub fn new(value: T) -> Gc<T> {
        let boxed = Box::new(GcBox {
            mark: Cell::new(false),
            data: value,
        });
        let raw = Box::into_raw(boxed);
        with_heap_mut(|heap| heap.register(raw as *mut GcBox<dyn Trace>));
        let gc = Gc {
            ptr: unsafe { NonNull::new_unchecked(raw) },
            _not_send_sync: std::marker::PhantomData,
        };
        // GC-2 `--gc-stress`: the fresh box is not yet reachable from any
        // handle the caller holds, so it is passed through as an extra root.
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

/// The thread-local mark-sweep heap.
pub struct Heap {
    live: Vec<*mut GcBox<dyn Trace>>,
}

thread_local! {
    static HEAP: RefCell<Heap> = const { RefCell::new(Heap::new()) };
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
}

/// Record that a traced `RefCell` was borrowed during marking; the sweep
/// must be aborted.
pub fn note_aborted_trace() {
    ABORT_SWEEP.with(|abort| abort.set(true));
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
        let by_addr: HashMap<usize, *mut GcBox<dyn Trace>> = heap
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
        Heap { live: Vec::new() }
    }

    /// Number of live boxes (for the leak-detection harness).
    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    fn register(&mut self, boxed: *mut GcBox<dyn Trace>) {
        self.live.push(boxed);
    }

    /// Mark-sweep from `roots`. Reachable boxes are kept (and unmarked for the
    /// next cycle); everything else is dropped and its memory freed. Marking
    /// is iterative (an explicit worklist), so a deeply nested object graph
    /// (a long rope, a deep prototype chain) cannot overflow the native
    /// stack.
    pub fn collect(&mut self, roots: &[GcAny]) {
        let work = roots.to_vec();
        self.collect_from_work(work);
    }
}

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
    pub fn collect_with_stack(&mut self, roots: &[GcAny]) {
        // Address → fat pointer: the scan recovers a box address from a
        // stack word and needs the `dyn Trace` vtable to mark through it.
        let by_addr: HashMap<usize, *mut GcBox<dyn Trace>> = self
            .live
            .iter()
            .map(|ptr| (*ptr as *const u8 as usize, *ptr))
            .collect();
        let sp = &by_addr as *const HashMap<usize, *mut GcBox<dyn Trace>> as usize;
        let mut work: Vec<GcAny> = roots.to_vec();
        if let Some((_low, high)) = stack_bounds()
            && high > sp
        {
            self.scan_stack(sp, high, &by_addr, &mut work);
        }
        self.collect_from_work(work);
    }

    /// The mark phase shared by [`Heap::collect`] and
    /// [`Heap::collect_with_stack`]: drain `work` (the roots plus any boxes
    /// the conservative stack scan found) iteratively, then sweep.
    fn collect_from_work(&mut self, mut work: Vec<GcAny>) {
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
            return;
        }
        let mut keep = Vec::with_capacity(self.live.len());
        for ptr in self.live.drain(..) {
            // SAFETY: every entry was registered by `Gc::new` and is a valid
            // box; unmarked boxes have no live handles (the rooting
            // discipline), so dropping them cannot dangle a handle.
            unsafe {
                if (*ptr).mark.get() {
                    (*ptr).mark.set(false);
                    keep.push(ptr);
                } else {
                    drop(Box::from_raw(ptr));
                }
            }
        }
        self.live = keep;
    }

    /// Scan every word in the current thread's live stack region
    /// `[sp, high)` and push boxes whose address appears there onto `work`.
    /// `by_addr` maps every registered box address to its fat pointer; an
    /// address can only be marked when it is a real box, so coincidental
    /// stack values at worst retain a reachable box (imprecise, never
    /// unsafe).
    fn scan_stack(
        &self,
        sp: usize,
        high: usize,
        by_addr: &HashMap<usize, *mut GcBox<dyn Trace>>,
        work: &mut Vec<GcAny>,
    ) {
        let mut addr = sp;
        while addr < high {
            // SAFETY: `[sp, high)` is the current thread's committed stack
            // (platform stack_bounds guarantees readability); reads are
            // unaligned so the exact frame layout does not matter.
            let word = unsafe { std::ptr::read_unaligned::<usize>(addr as *const usize) };
            if let Some(&ptr) = by_addr.get(&word) {
                // SAFETY: `ptr` is a registered, live box; the scan only
                // pushes boxes already in the live set.
                work.push(GcAny(ptr));
            } else if let Some(box_addr) = crate::value::Value::encoded_box_address(word as u64)
                && let Some(&ptr) = by_addr.get(&box_addr)
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
