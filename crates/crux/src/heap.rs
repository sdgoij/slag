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
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

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
pub struct Gc<T: Trace> {
    ptr: NonNull<GcBox<T>>,
    _not_send_sync: std::marker::PhantomData<*mut ()>,
}

impl<T: Trace> Copy for Gc<T> {}
impl<T: Trace> Clone for Gc<T> {
    fn clone(&self) -> Self {
        *self
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
        Gc {
            ptr: unsafe { NonNull::new_unchecked(raw) },
            _not_send_sync: std::marker::PhantomData,
        }
    }

    /// The erased form, usable as a [`Heap::collect`] root.
    pub fn as_any(self) -> GcAny {
        GcAny(self.ptr.as_ptr() as *mut GcBox<dyn Trace>)
    }
}

impl<T: Trace> Deref for Gc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // The box is kept alive by the rooting discipline (see the module
        // docs): a live handle must be marked at sweep time.
        unsafe { &self.ptr.as_ref().data }
    }
}

impl<T: Trace> DerefMut for Gc<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut self.ptr.as_mut().data }
    }
}

impl<T: Trace> Trace for Gc<T> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        visit(self.as_any());
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
        self.inner.borrow().trace(visit);
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

impl<T: Trace> Trace for Option<Gc<T>> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        if let Some(gc) = self {
            gc.trace(visit);
        }
    }
}

impl<T: Trace> Trace for Vec<Gc<T>> {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        for gc in self {
            gc.trace(visit);
        }
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
    /// next cycle); everything else is dropped and its memory freed.
    pub fn collect(&mut self, roots: &[GcAny]) {
        for root in roots {
            self.mark(*root);
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

    fn mark(&self, any: GcAny) {
        // SAFETY: `any` came from a live `Gc<T>`'s `Trace` impl, so the box
        // is allocated; the mark bit breaks cycles in the traversal.
        unsafe {
            let ptr = any.0;
            if (*ptr).mark.get() {
                return;
            }
            (*ptr).mark.set(true);
            (*ptr).data.trace(&mut |child| self.mark(child));
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
}
