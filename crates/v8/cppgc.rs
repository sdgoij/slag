//! The embedder's own heap (`v8::cppgc`).
//!
//! A host uses this to keep Rust objects that JavaScript retains: a value that
//! knows how to trace itself, a JS wrapper the host attaches it to, and handles
//! that keep it alive from outside the engine's heap. The engine does have a
//! host-object seam (`crux::host::HostOps`), but it dispatches *behaviour* and
//! carries no heap edges, so nothing here rides on it: the heap below is the
//! bridge's own.
//!
//! # The tier: a real heap that never collects
//!
//! Real, and tested: allocation (the value lives in a box this heap owns and
//! frees), every handle type (`UnsafePtr`, `Member`, `WeakMember`, `Persistent`,
//! `WeakPersistent`, `GcCell`), tracing (`Visitor::trace` calls a host's
//! `GarbageCollected::trace` and records what it reached; the mark phase walks
//! that transitively from the heap's roots), the JS association (`Object::wrap`
//! and `Object::unwrap` round-trip the same pointer through a JS object), and
//! reclamation at the end of the heap's life (`terminate`, or the drop that
//! follows it, drops every value).
//!
//! Not real: **no collection runs.** Nothing is reclaimed before the heap is
//! terminated, so `WeakPersistent::get` never observes a collection and
//! `collect_garbage_for_testing` traces without sweeping. That is a decision, not
//! an unfinished edge. A sweep has to know every pointer a host still holds, and
//! V8's cppgc answers that by scanning the stack — which Slag does not do for host
//! memory (`crux::heap`'s scan finds its own boxes). Freeing an object a stack
//! `UnsafePtr` still names would hand the host a dangling pointer: the aliasing
//! hazard `crates/crux/src/heap.rs` documents for engine handles, one level up.
//! So the honest tier is "real heap, no collector", and a host that needs
//! reclamation needs an engine that traces host state (`.notes/embedding.md` §5,
//! level L2).
//!
//! # Divergences a host can observe
//!
//! - **The wrap is a property, not an embedder field.** V8 keeps a wrapped
//!   pointer where JavaScript cannot see it; this bridge defines an
//!   [`External`](crate::External) under a bridge-owned key instead — hidden from
//!   `for-in`, `Object.keys` and `JSON.stringify` (the descriptor is
//!   non-enumerable), and non-configurable so no program can remove a wrap. It
//!   *is* visible to `Object.getOwnPropertyNames`, and a program that defines the
//!   same key itself can make [`Local::is_api_wrapper`] answer `true` — but it
//!   cannot make [`Object::unwrap`] answer anything, because only the bridge can
//!   make an `External`. For the same reason a second `Object::wrap` of one
//!   wrapper under one tag is refused rather than silently overwriting the first,
//!   which is where the crate we stand in for does overwrite.
//! - **Weak is traced like strong.** Clearing a weak pointer is a sweep's job and
//!   no sweep runs, so the two only differ in a tier that collects.
//! - **`Isolate::get_cpp_heap` always answers `Some`.** V8 answers null unless the
//!   embedder handed `CreateParams` a heap; the host this stands in for unwraps
//!   the answer, so an isolate here always owns one.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashSet;
use std::ffi::CStr;
use std::fmt;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crux::error::{ErrorKind, JsError};
use crux::object::JsObject;
use crux::property::{PropertyDescriptor as EngineDescriptor, PropertyKey};
use crux::value::Value as EngineValue;
use runtime::api;

use crate::data::Object;
use crate::handle::{Local, LocalHandle};
use crate::platform::Platform;
use crate::support::{SharedRef, UniqueRef};

/// The prefix of every allocation, and the whole of what a `*mut RustObj` names.
///
/// The value the header belongs to follows it in the same box, which is the
/// layout the C++ heap used: a pointer to a managed object is a pointer to a
/// header, and the type is recovered from the pointer a caller passes rather
/// than stored. `#[doc(hidden)] pub` for the reason the crate we stand in for
/// keeps it in a private module — [`GetRustObj`] names it in a public signature,
/// and a host spells neither.
#[doc(hidden)]
#[repr(C)]
pub struct RustObj {
    /// The value in this allocation, erased. `trace` and `get_name` are called
    /// through it, which is what lets the mark phase walk from an object to what
    /// it holds without knowing the type. `None` only between a box being made
    /// and its value having an address, which is inside [`Heap::allocate`].
    dynamic: Option<NonNull<dyn GarbageCollected>>,
    /// Frees this allocation at the concrete type: from an erased pointer,
    /// `Box::from_raw` needs the `T` that this carries instead.
    free: unsafe fn(*mut RustObj),
    /// How many live [`Persistent`] handles name this object — the root set the
    /// mark phase starts from.
    roots: Cell<usize>,
}

/// A managed value: its header, then the value itself.
#[repr(C)]
struct ObjBox<T> {
    head: RustObj,
    value: T,
}

/// The value `obj` allocates.
///
/// # Safety
///
/// `obj` must have come from [`Heap::allocate`] for the same `T`.
unsafe fn value_at<T: GarbageCollected>(obj: *mut RustObj) -> *mut T {
    // SAFETY: the caller's contract — the box was made for this `T`, so its
    // layout is `ObjBox<T>` and the value is the field after the header.
    unsafe { std::ptr::addr_of_mut!((*obj.cast::<ObjBox<T>>()).value) }
}

/// Drop the value in `obj` and free its box.
///
/// # Safety
///
/// `obj` must have come from [`Heap::allocate`] for the same `T`, and this must
/// be the only call for it.
unsafe fn free_box<T: GarbageCollected>(obj: *mut RustObj) {
    // SAFETY: the caller's contract.
    drop(unsafe { Box::from_raw(obj.cast::<ObjBox<T>>()) });
}

/// What a trace runs with (`v8::cppgc::Visitor`).
///
/// V8 hands one to a host's `trace` method and marks through it; here it holds
/// the mark stack and the set of objects already reached, which is what makes a
/// trace terminate on a cycle and visit an object once however many edges name
/// it. A host never builds one — [`Heap`] builds it for its mark phase — which
/// is why the only thing a `Visitor` can be asked to do is trace.
pub struct Visitor {
    /// Objects reached but not traced yet.
    worklist: Vec<NonNull<RustObj>>,
    /// The addresses in the worklist, or already traced: an object is traced
    /// once, so the worklist cannot grow without bound on a cycle.
    reached: HashSet<usize>,
}

impl Visitor {
    /// Trace a managed object (`Visitor::Trace`).
    ///
    /// A host calls this from its `GarbageCollected::trace` for every `Member`,
    /// `WeakMember` and `GcCell` the object holds. An edge a host does not trace
    /// is an edge the collector cannot see, which is the contract the trait
    /// states.
    #[inline(always)]
    pub fn trace(&mut self, member: &impl Traced) {
        member.trace(self);
    }

    fn new() -> Self {
        Self {
            worklist: Vec::new(),
            reached: HashSet::new(),
        }
    }

    /// Note that `pointer` is live, unless it is the null pointer or already was.
    fn reach(&mut self, pointer: *mut RustObj) {
        if pointer.is_null() {
            return;
        }
        if self.reached.insert(pointer as usize) {
            // SAFETY: the null case returned above, and a handle's pointer always
            // came from an allocation.
            self.worklist
                .push(unsafe { NonNull::new_unchecked(pointer) });
        }
    }

    /// Trace everything reached, transitively: pop an object, run the host's
    /// `trace` for it, and repeat until the worklist is empty.
    fn run(&mut self) {
        while let Some(pointer) = self.worklist.pop() {
            // SAFETY: the pointer came from an allocation, and nothing is freed
            // before the heap is terminated, so the value is still there; its
            // `dynamic` was written by `allocate`.
            let dynamic = unsafe { (*pointer.as_ptr()).dynamic };
            if let Some(dynamic) = dynamic {
                // SAFETY: as above, and `allocate` only ever stored a pointer to
                // a value that is still live.
                unsafe { dynamic.as_ref() }.trace(self);
            }
        }
    }

    /// How many objects this trace reached.
    fn reached(&self) -> usize {
        self.reached.len()
    }
}

/// An inlined object that follows the managed-heap layout without being
/// allocated on its own (`v8::cppgc::Traced`).
pub trait Traced {
    /// Called by the `Visitor` when tracing managed objects.
    fn trace(&self, visitor: &mut Visitor);
}

impl<T: Traced> Traced for Option<T> {
    fn trace(&self, visitor: &mut Visitor) {
        if let Some(value) = self {
            value.trace(visitor);
        }
    }
}

/// A type whose instances live on the heap (`v8::cppgc::GarbageCollected`).
///
/// # Safety
///
/// `trace` must call [`Visitor::trace`] for every `Member`, `WeakMember` and
/// `GcCell` the object reaches. A missed edge cannot dangle here, because nothing
/// is reclaimed — it does make the mark phase report less than the object holds,
/// and that report is what a host's own tests are written against.
pub unsafe trait GarbageCollected {
    /// `trace` must call [`Visitor::trace`] for each [`Member`], [`WeakMember`]
    /// or [`Traced`] reachable from `self`.
    fn trace(&self, visitor: &mut Visitor);

    /// A name for the object, which a heap dump would show. V8 may call this at
    /// any time and while taking a snapshot, so the string must live forever.
    fn get_name(&self) -> &'static CStr;
}

/// The object pointer behind a handle (the crate we stand in for's
/// `GetRustObj`).
///
/// This is what lets the pointer types here compose: a `Persistent` is made from
/// an `UnsafePtr`, a `Member` from a `Persistent`, and `Object::wrap` takes any
/// of them.
#[doc(hidden)]
pub trait GetRustObj<T: GarbageCollected> {
    fn get_rust_obj(&self) -> *mut RustObj;
}

impl<T: GarbageCollected> GetRustObj<T> for *mut RustObj {
    fn get_rust_obj(&self) -> *mut RustObj {
        *self
    }
}

/// A heap for allocated host objects (`v8::cppgc::Heap`).
///
/// Like an isolate, it is used from one thread at a time. It owns every
/// allocation made on it and frees them all when it is terminated — nothing
/// before that, which is the tier [`self`]'s module header states.
pub struct Heap {
    /// Every allocation this heap made, in the order it made them: read by the
    /// mark phase (for the roots among them) and drained by `terminate` and the
    /// `Drop` that follows it.
    allocations: RefCell<Vec<NonNull<RustObj>>>,
}

impl Heap {
    /// A heap attached to `platform` (`cppgc::Heap::Create`).
    ///
    /// Neither argument changes anything here and both are taken for the shape's
    /// sake: a platform schedules cppgc's concurrent marking and sweeping, and
    /// this heap does neither.
    pub fn create(platform: SharedRef<Platform>, params: HeapCreateParams) -> UniqueRef<Heap> {
        let _ = (platform, params);
        UniqueRef::new(Self::new())
    }

    /// An empty heap, for the isolate that always owns one (see
    /// [`Isolate::get_cpp_heap`](crate::Isolate::get_cpp_heap)).
    pub(crate) fn new() -> Self {
        Self {
            allocations: RefCell::new(Vec::new()),
        }
    }

    /// Mark from the heap's roots
    /// (`v8::cppgc::Heap::collect_garbage_for_testing`).
    ///
    /// The mark phase is real: every object a live [`Persistent`] names is traced
    /// transitively, and a host's `trace` methods run for each object reached.
    /// What does not happen is the sweep, so this reclaims nothing — the module
    /// header says why rather than leaving a host to discover it.
    ///
    /// # Safety
    ///
    /// As there: the caller must ensure no object it still names has become
    /// unreachable.
    pub unsafe fn collect_garbage_for_testing(&self, stack_state: EmbedderStackState) {
        let _ = stack_state;
        // The reachability the mark computes has nowhere to go: the sweep that
        // would use it is the part of a collector this tier does not have.
        let _reached = self.mark();
    }

    /// Allow collections while the heap is detached from an isolate
    /// (`v8::cppgc::Heap::enable_detached_garbage_collections_for_testing`).
    ///
    /// Accepted, and of no effect: it exists so a host's test setup can run its
    /// sequence, and what it turns on there is a detached sweep, which this tier
    /// does not have.
    pub fn enable_detached_garbage_collections_for_testing(&self) {}

    /// Stop the heap and release its memory (`v8::cppgc::Heap::Terminate`).
    ///
    /// Every allocation is dropped, so every value's destructor runs. A pointer
    /// into the heap is dangling afterwards — as there, where this call is
    /// documented to be followed by destroying the heap.
    pub fn terminate(&mut self) {
        self.free_all();
    }

    /// Allocate `obj` on this heap and register it.
    fn allocate<T: GarbageCollected + 'static>(&self, obj: T) -> UnsafePtr<T> {
        let mut boxed = Box::new(ObjBox {
            head: RustObj {
                // Written below: `Box::new` moves the value into the allocation,
                // so the value's address is not known until it is there.
                dynamic: None,
                free: free_box::<T>,
                roots: Cell::new(0),
            },
            value: obj,
        });
        let dynamic: NonNull<dyn GarbageCollected> = NonNull::from(&mut boxed.value);
        boxed.head.dynamic = Some(dynamic);
        let pointer = NonNull::new(Box::into_raw(boxed).cast::<RustObj>())
            .expect("cppgc: a box is never the null pointer");
        self.allocations.borrow_mut().push(pointer);
        UnsafePtr {
            pointer,
            _phantom: PhantomData,
        }
    }

    /// Trace every object reachable from the heap's roots, once each, and answer
    /// how many that was.
    ///
    /// A host never calls this directly; it is what
    /// [`collect_garbage_for_testing`](Self::collect_garbage_for_testing) runs.
    fn mark(&self) -> usize {
        // The roots are read out before tracing starts, so a host's `trace` that
        // allocates does not find the list borrowed. An allocation made during a
        // mark is simply not part of it.
        let roots: Vec<*mut RustObj> = {
            let allocations = self.allocations.borrow();
            allocations
                .iter()
                .filter(|allocation| {
                    // SAFETY: every pointer in the list came from `allocate`.
                    (unsafe { allocation.as_ref().roots.get() }) > 0
                })
                .map(|allocation| allocation.as_ptr())
                .collect()
        };
        let mut visitor = Visitor::new();
        for root in roots {
            visitor.reach(root);
        }
        visitor.run();
        visitor.reached()
    }

    /// Drop every value and free every allocation.
    fn free_all(&mut self) {
        for pointer in self.allocations.get_mut().drain(..) {
            // SAFETY: the pointer came from `allocate`, which stored the matching
            // free function, and the list is drained so this runs once for it.
            unsafe { (pointer.as_ref().free)(pointer.as_ptr()) };
        }
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        self.free_all();
    }
}

impl fmt::Debug for Heap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Heap(..)")
    }
}

/// Whether the stack may hold pointers into the heap
/// (`cppgc::EmbedderStackState`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedderStackState {
    /// The stack may contain interesting heap pointers.
    MayContainHeapPointers,
    /// The stack does not contain any interesting heap pointers.
    NoHeapPointers,
}

/// What marking a heap may do (`cppgc::MarkingType`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkingType {
    /// Atomic stop-the-world marking: no write barriers, the most intrusive.
    Atomic,
    /// Incremental marking, interleaved with the application on this thread.
    Incremental,
    /// Incremental and concurrent marking.
    IncrementalAndConcurrent,
}

/// What sweeping a heap may do (`cppgc::SweepingType`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepingType {
    /// Atomic stop-the-world sweeping.
    Atomic,
    /// Incremental sweeping, interleaved with the application.
    Incremental,
    /// Incremental and concurrent sweeping.
    IncrementalAndConcurrent,
}

/// How a heap is created (`cppgc::HeapCreateParams`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapCreateParams {
    /// Which kinds of marking the heap supports.
    pub marking_support: MarkingType,
    /// Which kinds of sweeping the heap supports.
    pub sweeping_support: SweepingType,
}

impl Default for HeapCreateParams {
    fn default() -> Self {
        Self {
            marking_support: MarkingType::IncrementalAndConcurrent,
            sweeping_support: SweepingType::IncrementalAndConcurrent,
        }
    }
}

/// The index of an embedder field (`v8::cppgc::InternalFieldIndex` in the crate
/// we stand in for).
pub type InternalFieldIndex = i32;

/// Process-global initialization of the host-object collector
/// (`cppgc::InitializeProcess`).
///
/// Accepted, and nothing to do: there is no process-global collector here — a
/// heap is a list of allocations, and the engine's collector is the engine's —
/// so a host's startup sequence runs and no state is set up. It is taken rather
/// than omitted because that sequence makes the call, and because a heap there
/// may be created only after it.
pub fn initialize_process(platform: SharedRef<Platform>) {
    let _ = platform;
}

/// Tear down what [`initialize_process`] set up
/// (`cppgc::ShutdownProcess`).
///
/// # Safety
///
/// Must be called after destroying the last heap, as there. Nothing here holds
/// process-global metadata to release, so the contract is entirely the caller's.
pub unsafe fn shutdown_process() {}

/// Construct a value on the heap (`cppgc::MakeGarbageCollected`).
///
/// The object is allocated on `heap` and owned by it: its destructor runs when
/// the heap is terminated, and `trace` is called on it during a mark.
///
/// # Safety
///
/// The returned pointer must stay on the stack or move into one of the pointer
/// types here, exactly as there: a `Persistent` roots it, a `Member` field
/// traces it, and a bare `UnsafePtr` living in host memory is a pointer nothing
/// knows about.
pub unsafe fn make_garbage_collected<T: GarbageCollected + 'static>(
    heap: &Heap,
    obj: T,
) -> UnsafePtr<T> {
    heap.allocate(obj)
}

/// A pointer to an object on the heap, from the stack
/// (`v8::cppgc::UnsafePtr`).
///
/// Not `#[derive(Copy)]`: a derive would demand `T: Copy` of the value it
/// points at, which the pointer has no reason to care about.
pub struct UnsafePtr<T: GarbageCollected> {
    pointer: NonNull<RustObj>,
    _phantom: PhantomData<T>,
}

impl<T: GarbageCollected> Clone for UnsafePtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: GarbageCollected> Copy for UnsafePtr<T> {}

impl<T: GarbageCollected> UnsafePtr<T> {
    /// A pointer to the object `value` names, `None` for the null pointer
    /// (`v8::cppgc::UnsafePtr::new`).
    ///
    /// # Safety
    ///
    /// `T` must be the type the object was allocated as: a pointer carries no
    /// record of it, and this is where the caller states it.
    pub unsafe fn new(value: &impl GetRustObj<T>) -> Option<UnsafePtr<T>> {
        NonNull::new(value.get_rust_obj()).map(|pointer| UnsafePtr {
            pointer,
            _phantom: PhantomData,
        })
    }

    /// The value, borrowed (`v8::cppgc::UnsafePtr::as_ref`).
    ///
    /// # Safety
    ///
    /// `T` must be the type this pointer was made for, and the heap must not
    /// have been terminated since.
    pub unsafe fn as_ref(&self) -> &T {
        // SAFETY: the caller's contract.
        unsafe { &*value_at::<T>(self.pointer.as_ptr()) }
    }
}

impl<T: GarbageCollected> GetRustObj<T> for UnsafePtr<T> {
    fn get_rust_obj(&self) -> *mut RustObj {
        self.pointer.as_ptr()
    }
}

macro_rules! member {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        pub struct $name<T: GarbageCollected> {
            /// The object this names; null for an empty one.
            pointer: *mut RustObj,
            _phantom: PhantomData<T>,
        }

        impl<T: GarbageCollected> $name<T> {
            /// An empty member, to be set later.
            pub fn empty() -> Self {
                Self {
                    pointer: std::ptr::null_mut(),
                    _phantom: PhantomData,
                }
            }

            /// A member naming `other`.
            pub fn new(other: &impl GetRustObj<T>) -> Self {
                Self {
                    pointer: other.get_rust_obj(),
                    _phantom: PhantomData,
                }
            }

            /// Point this member at `other`.
            ///
            /// The crate we stand in for routes this through C++ because a store
            /// into a managed heap takes a write barrier; here the store is the
            /// pointer and nothing else, because marking never runs across one.
            pub fn set(&mut self, other: &impl GetRustObj<T>) {
                self.pointer = other.get_rust_obj();
            }

            /// The object this names, `None` for an empty member.
            ///
            /// # Safety
            ///
            /// The holder of this member must trace it, as there: an untraced
            /// member is an edge the mark phase cannot see.
            pub unsafe fn get(&self) -> Option<&T> {
                if self.pointer.is_null() {
                    return None;
                }
                // SAFETY: the caller's contract, and the empty case returned.
                Some(unsafe { &*value_at::<T>(self.pointer) })
            }
        }

        impl<T: GarbageCollected> GetRustObj<T> for $name<T> {
            fn get_rust_obj(&self) -> *mut RustObj {
                self.pointer
            }
        }

        impl<T: GarbageCollected> Traced for $name<T> {
            fn trace(&self, visitor: &mut Visitor) {
                visitor.reach(self.pointer);
            }
        }

        impl<T: GarbageCollected> fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name)).finish()
            }
        }
    };
}

member!(
    Member,
    "A strong pointer to another object on the heap, held as a field of one\n(`v8::cppgc::Member`).\n\nEvery `Member` field must be traced in its holder's `trace`, which is what keeps\nwhat it points at reachable."
);
member!(
    WeakMember,
    "A weak pointer to another object on the heap (`v8::cppgc::WeakMember`).\n\nA weak member does not keep its object alive, and the collector clears it when\nthe object dies. Nothing dies here before the heap is terminated, so a weak\nmember answers what a strong one does — the difference is a sweep's to make."
);

macro_rules! persistent {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        pub struct $name<T: GarbageCollected> {
            /// The object this roots; null for an empty handle.
            pointer: *mut RustObj,
            _phantom: PhantomData<T>,
        }

        impl<T: GarbageCollected> $name<T> {
            /// An empty handle, to be set later.
            pub fn empty() -> Self {
                Self {
                    pointer: std::ptr::null_mut(),
                    _phantom: PhantomData,
                }
            }

            /// A handle rooting `other`.
            pub fn new(other: &impl GetRustObj<T>) -> Self {
                let pointer = other.get_rust_obj();
                Self::retain(pointer);
                Self {
                    pointer,
                    _phantom: PhantomData,
                }
            }

            /// Root `other` instead of what this names.
            pub fn set(&mut self, other: &impl GetRustObj<T>) {
                let pointer = other.get_rust_obj();
                if pointer == self.pointer {
                    return;
                }
                Self::release(self.pointer);
                Self::retain(pointer);
                self.pointer = pointer;
            }

            /// The object this roots, `None` for an empty handle.
            ///
            /// Always `Some` for a `WeakPersistent` too, while the object was
            /// allocated and the heap is alive: a weak handle would answer `None`
            /// once its object was collected, and nothing is collected here.
            pub fn get(&self) -> Option<&T> {
                if self.pointer.is_null() {
                    return None;
                }
                // SAFETY: the pointer names a live allocation — nothing is freed
                // before the heap is terminated.
                Some(unsafe { &*value_at::<T>(self.pointer) })
            }

            fn retain(pointer: *mut RustObj) {
                if pointer.is_null() {
                    return;
                }
                // SAFETY: a handle's pointer names an allocation of a live heap.
                let roots = unsafe { &(*pointer).roots };
                roots.set(roots.get() + 1);
            }

            fn release(pointer: *mut RustObj) {
                if pointer.is_null() {
                    return;
                }
                // SAFETY: as `retain`.
                let roots = unsafe { &(*pointer).roots };
                roots.set(roots.get().checked_sub(1).expect(
                    "cppgc: releasing a root this object never had — a handle was copied without a new one being taken",
                ));
            }
        }

        impl<T: GarbageCollected> Drop for $name<T> {
            fn drop(&mut self) {
                Self::release(self.pointer);
            }
        }

        impl<T: GarbageCollected> GetRustObj<T> for $name<T> {
            fn get_rust_obj(&self) -> *mut RustObj {
                self.pointer
            }
        }

        impl<T: GarbageCollected> fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name)).finish()
            }
        }
    };
}

persistent!(
    Persistent,
    "A strong pointer from off the heap to an object on it\n(`v8::cppgc::Persistent`).\n\nAs long as the handle is alive the mark phase has the object as a root. It is\ncreated and dropped on one thread."
);
persistent!(
    WeakPersistent,
    "A weak pointer from off the heap to an object on it\n(`v8::cppgc::WeakPersistent`).\n\nA weak handle does not root its object: the collector would clear it when the\nobject died. No object dies before the heap is terminated, so this handle keeps\nanswering the object it was made from."
);

/// A memory cell with interior mutability for a managed object
/// (`v8::cppgc::GcCell`).
///
/// Mutable access is granted by proof of access to the isolate, which is what
/// statically keeps the access patterns inside what the collector allows: a
/// `GcCell` holding other managed objects is read through the isolate that owns
/// them, and while it is borrowed no nested object can be read through the same
/// borrow.
pub struct GcCell<T> {
    value: UnsafeCell<T>,
}

impl<T> GcCell<T> {
    pub fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
        }
    }

    pub fn set(&self, isolate: &mut crate::Isolate, value: T) {
        let _ = isolate;
        // SAFETY: the `isolate` argument is the proof that this is the only
        // access to the cell.
        unsafe { *self.value.get() = value };
    }

    pub fn get<'a>(&'a self, isolate: &'a crate::Isolate) -> &'a T {
        let _ = isolate;
        // SAFETY: the `isolate` argument is the proof of access, and the returned
        // reference binds the isolate's lifetime.
        unsafe { &*self.value.get() }
    }

    pub fn get_mut<'a>(&'a self, isolate: &'a mut crate::Isolate) -> &'a mut T {
        let _ = isolate;
        // SAFETY: as `get`, with an exclusive borrow as the proof.
        unsafe { &mut *self.value.get() }
    }

    pub fn with<'a, 's, 'i, R>(
        &'a self,
        scope: &'a mut crate::PinScope<'s, 'i>,
        f: impl FnOnce(&'a mut crate::PinScope<'s, 'i>, &'a mut T) -> R,
    ) -> R {
        // SAFETY: the scope is the proof of access, as the isolate is in the
        // other accessors; the crate we stand in for takes a `&mut HandleScope`
        // here, and a `HandleScope` in this bridge is always pinned — a
        // `PinScope` is that pinned scope, which is why it is the parameter.
        f(scope, unsafe { &mut *self.value.get() })
    }
}

// SAFETY: the cell is a plain `UnsafeCell<T>`, and the access proof is the
// isolate, which is single-threaded — the same reasoning the crate we stand in
// for states.
unsafe impl<T: Send> Send for GcCell<T> {}
unsafe impl<T: Sync> Sync for GcCell<T> {}

impl<T> fmt::Debug for GcCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcCell").finish()
    }
}

impl<T: Traced> Traced for GcCell<T> {
    fn trace(&self, visitor: &mut Visitor) {
        // SAFETY: a trace runs under the collector's own rules, so reading the
        // cell here is the access the collector has.
        visitor.trace(unsafe { &*self.value.get() });
    }
}

/// The own-property key that says an object has been wrapped, whatever the tag.
const WRAP_MARKER: &str = "*cppgc.wrap*";

/// The own-property key prefix a wrapped pointer is stored under.
const WRAP_PREFIX: &str = "*cppgc.wrap.";

/// The key an object wrapped under `tag` carries its pointer at.
fn wrap_key(tag: u16) -> PropertyKey {
    PropertyKey::from_utf8(&format!("{WRAP_PREFIX}{tag}*"))
}

/// The key that says an object is wrapped at all.
fn wrap_marker_key() -> PropertyKey {
    PropertyKey::from_utf8(WRAP_MARKER)
}

/// The pointer `wrapper` carries under `tag`, if it carries one.
fn wrapped_pointer(wrapper: &Local<'_, Object>, tag: u16) -> Option<*mut RustObj> {
    let object = wrapper.engine().as_object()?;
    // A lookup that threw leaves the exception pending and answers `None` here:
    // this signature has no error channel, and the host's own boundary is where a
    // pending exception is reported.
    let property = object.get_own_property_key(&wrap_key(tag)).ok()??;
    let value = property.value()?;
    // Anything that is not an `External` reads as the null pointer, and only the
    // bridge can make one — so a program that defines this key itself cannot
    // make an unwrap answer a pointer.
    let pointer = api::External::from(value).value().cast::<RustObj>();
    if pointer.is_null() {
        None
    } else {
        Some(pointer)
    }
}

impl Object {
    /// Attach a managed object to a JS wrapper (`v8::Object::Wrap`).
    ///
    /// The wrapper carries the pointer at a bridge-owned key, which is how this
    /// bridge stands in for the embedder field V8 would store it in (see the
    /// module header). A wrapper that already carries a pointer under `TAG` is
    /// refused rather than overwritten, because the key is non-configurable —
    /// there, a second `Wrap` replaces the first.
    ///
    /// # Safety
    ///
    /// `TAG` must be unique to the caller within the heap, as there: an unwrap
    /// with a tag the wrapper was not wrapped under answers `None`, and two
    /// callers sharing one tag would read each other's objects.
    pub unsafe fn wrap<const TAG: u16, T: GarbageCollected>(
        isolate: &mut crate::Isolate,
        wrapper: Local<'_, Object>,
        value: &impl GetRustObj<T>,
    ) {
        let Some(object) = wrapper.engine().as_object() else {
            panic!("v8::Object::wrap: the wrapper is not an object");
        };
        match object.get_own_property_key(&wrap_key(TAG)) {
            Ok(Some(_)) => {
                crate::throw(
                    isolate,
                    &JsError::new(
                        ErrorKind::TypeError,
                        format!(
                            "v8::Object::wrap: this wrapper already carries a pointer under tag {TAG}"
                        ),
                    ),
                );
                return;
            }
            Ok(None) => {}
            Err(error) => {
                crate::throw(isolate, &error);
                return;
            }
        }

        let external = EngineValue::Object(JsObject::external_object_create(
            value.get_rust_obj() as usize,
            None,
        ));
        if let Err(error) =
            object.define_property_key(&wrap_key(TAG), &EngineDescriptor::none(external))
        {
            // Thrown rather than dropped: a wrap that did not happen is a wrapper
            // the host cannot unwrap later, and it has no return value to say so.
            crate::throw(isolate, &error);
            return;
        }

        // The marker is separate because `is_api_wrapper` has no tag to look up.
        // It is written once per object, and a marker that cannot be written
        // leaves a wrapper `is_api_wrapper` would deny — so it is reported too.
        match object.get_own_property_key(&wrap_marker_key()) {
            Ok(marked) => {
                if marked.is_none()
                    && let Err(error) = object.define_property_key(
                        &wrap_marker_key(),
                        &EngineDescriptor::none(EngineValue::Undefined),
                    )
                {
                    crate::throw(isolate, &error);
                }
            }
            Err(error) => crate::throw(isolate, &error),
        }
    }

    /// The managed object a wrapper carries under `TAG`
    /// (`v8::Object::Unwrap`), `None` for an object that was not wrapped under
    /// that tag.
    ///
    /// # Safety
    ///
    /// `T` must be the type the pointer was wrapped as, and the returned pointer
    /// must stay on the stack or move into one of this module's pointer types.
    pub unsafe fn unwrap<const TAG: u16, T: GarbageCollected>(
        _isolate: &mut crate::Isolate,
        wrapper: Local<'_, Object>,
    ) -> Option<UnsafePtr<T>> {
        let pointer = wrapped_pointer(&wrapper, TAG)?;
        // SAFETY: the caller's contract, and the pointer names a live allocation.
        unsafe { UnsafePtr::new(&pointer) }
    }
}

impl<'s> LocalHandle<'s, Object> {
    /// Whether this wrapper carries an object wrapped by [`Object::wrap`]
    /// (`v8::Object::is_api_wrapper`).
    ///
    /// The crate we stand in for answers "this object can hold a wrapped
    /// instance"; here the answer is "this bridge wrapped it", which is what a
    /// caller about to unwrap is asking. An object whose own-property lookup fails
    /// (a proxy trap that threw, a host object that refused) answers `false`: this
    /// predicate has no error channel, and the unwrap that follows fails the same
    /// way.
    pub fn is_api_wrapper(&self) -> bool {
        let Some(object) = self.engine().as_object() else {
            return false;
        };
        object
            .has_own_property_key(&wrap_marker_key())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::test_support::{bind, eval_number, in_context};

    /// A value that records its own destruction, so "the heap freed it" is
    /// observable rather than assumed, and that holds an edge, so a mark has
    /// something to follow.
    struct Tracked {
        edge: Member<Tracked>,
        freed: Option<&'static AtomicUsize>,
    }

    unsafe impl GarbageCollected for Tracked {
        fn trace(&self, visitor: &mut Visitor) {
            visitor.trace(&self.edge);
        }

        fn get_name(&self) -> &'static CStr {
            c"Tracked"
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            if let Some(freed) = self.freed {
                freed.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn tracked() -> Tracked {
        Tracked {
            edge: Member::empty(),
            freed: None,
        }
    }

    /// The object behind a pointer, mutably, the way a host's own field
    /// assignment reaches it.
    fn tracked_mut(pointer: UnsafePtr<Tracked>) -> &'static mut Tracked {
        // SAFETY: the test holds the only handle to this allocation.
        unsafe { &mut *value_at::<Tracked>(pointer.pointer.as_ptr()) }
    }

    /// The handles a host holds read the value the heap allocated, and a heap
    /// that is terminated drops what it allocated — once.
    #[test]
    fn a_heap_drops_what_it_allocated_when_it_is_terminated() {
        static FREED: AtomicUsize = AtomicUsize::new(0);
        let mut heap = Heap::new();
        let pointer = unsafe {
            make_garbage_collected(
                &heap,
                Tracked {
                    edge: Member::empty(),
                    freed: Some(&FREED),
                },
            )
        };

        // SAFETY: the pointer names the value just allocated, of this type.
        unsafe { assert_eq!(pointer.as_ref().get_name(), c"Tracked") };
        let root = Persistent::new(&pointer);
        assert_eq!(root.get().expect("rooted").get_name(), c"Tracked");
        assert_eq!(FREED.load(Ordering::SeqCst), 0);

        // The handles go before the heap does: a pointer into a terminated heap
        // is dangling, and dropping a handle reads the object it roots.
        drop(root);
        heap.terminate();
        assert_eq!(FREED.load(Ordering::SeqCst), 1);
        // Terminating twice is not freeing twice: there is nothing left to free.
        heap.terminate();
        assert_eq!(FREED.load(Ordering::SeqCst), 1);
    }

    /// Dropping a heap frees what `terminate` did not — the other way the memory
    /// comes back.
    #[test]
    fn dropping_a_heap_frees_what_terminate_did_not() {
        static DROPPED: AtomicUsize = AtomicUsize::new(0);
        {
            let heap = Heap::new();
            let _pointer = unsafe {
                make_garbage_collected(
                    &heap,
                    Tracked {
                        edge: Member::empty(),
                        freed: Some(&DROPPED),
                    },
                )
            };
            assert_eq!(DROPPED.load(Ordering::SeqCst), 0);
        }
        assert_eq!(DROPPED.load(Ordering::SeqCst), 1);
    }

    /// A handle starts empty, roots what it is set to, and moves to another
    /// object without rooting the one it left.
    #[test]
    fn a_handle_roots_what_it_names() {
        let heap = Heap::new();
        let first = unsafe { make_garbage_collected(&heap, tracked()) };
        let mut root = Persistent::<Tracked>::empty();
        assert!(root.get().is_none());
        assert_eq!(heap.mark(), 0);

        root.set(&first);
        // SAFETY: the pointer names an allocation, and T is its type.
        let rooted = unsafe { first.as_ref() } as *const Tracked;
        assert_eq!(root.get().expect("rooted") as *const Tracked, rooted);
        assert_eq!(heap.mark(), 1);

        let second = unsafe { make_garbage_collected(&heap, tracked()) };
        root.set(&second);
        assert_eq!(heap.mark(), 1, "still one root, a different object");
    }

    /// A pointer can be the null pointer, and a weak handle answers what it was
    /// made from while the heap holds the object.
    #[test]
    fn an_empty_pointer_and_an_empty_handle_name_nothing() {
        let heap = Heap::new();
        let null: *mut RustObj = std::ptr::null_mut();
        // SAFETY: the null pointer, which is the case the constructor answers
        // `None` for.
        assert!(unsafe { UnsafePtr::<Tracked>::new(&null) }.is_none());

        let member = Member::<Tracked>::empty();
        // SAFETY: an empty member names nothing.
        assert!(unsafe { member.get() }.is_none());
        let handle = WeakPersistent::<Tracked>::empty();
        assert!(handle.get().is_none());

        let pointer = unsafe { make_garbage_collected(&heap, tracked()) };
        let weak = WeakPersistent::new(&pointer);
        assert!(weak.get().is_some());
    }

    /// The mark phase starts at what a `Persistent` roots and follows `Member`
    /// edges from there — and an object nothing reaches is not in it.
    #[test]
    fn the_mark_phase_follows_member_edges_from_its_roots() {
        let heap = Heap::new();
        let first = unsafe { make_garbage_collected(&heap, tracked()) };
        let second = unsafe { make_garbage_collected(&heap, tracked()) };
        assert_eq!(heap.mark(), 0, "nothing is rooted yet");

        let root = Persistent::new(&first);
        assert_eq!(heap.mark(), 1, "the root itself");
        tracked_mut(first).edge = Member::new(&second);
        assert_eq!(heap.mark(), 2, "the root and what it points at");

        drop(root);
        assert_eq!(heap.mark(), 0, "the chain hangs off the root");
    }

    /// A cycle in the object graph terminates the trace and is counted once per
    /// object.
    #[test]
    fn the_mark_phase_terminates_on_a_cycle() {
        let heap = Heap::new();
        let first = unsafe { make_garbage_collected(&heap, tracked()) };
        let second = unsafe { make_garbage_collected(&heap, tracked()) };
        tracked_mut(first).edge = Member::new(&second);
        tracked_mut(second).edge = Member::new(&first);

        let _root = Persistent::new(&first);
        assert_eq!(heap.mark(), 2);
    }

    /// A `GcCell` holding a `Member` is traced through, which is the chain a
    /// host's own `trace` implementations walk.
    #[test]
    fn a_gc_cell_is_traced_through() {
        struct Holder {
            cell: GcCell<Option<Member<Tracked>>>,
        }

        unsafe impl GarbageCollected for Holder {
            fn trace(&self, visitor: &mut Visitor) {
                visitor.trace(&self.cell);
            }

            fn get_name(&self) -> &'static CStr {
                c"Holder"
            }
        }

        let heap = Heap::new();
        let held = unsafe { make_garbage_collected(&heap, tracked()) };
        let holder = unsafe {
            make_garbage_collected(
                &heap,
                Holder {
                    cell: GcCell::new(Some(Member::new(&held))),
                },
            )
        };
        assert_eq!(heap.mark(), 0, "nothing is rooted yet");

        let _root = Persistent::new(&holder);
        assert_eq!(heap.mark(), 2, "the root and the object its cell holds");
    }

    /// The cell's accessors go through the isolate they are handed, which is the
    /// proof that the access follows the collector's rules.
    #[test]
    fn a_gc_cell_is_read_and_written_through_an_isolate() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        let cell = GcCell::new(1u32);
        assert_eq!(*cell.get(isolate), 1);
        *cell.get_mut(isolate) = 2;
        assert_eq!(*cell.get(isolate), 2);
        cell.set(isolate, 3);
        assert_eq!(*cell.get(isolate), 3);

        crate::scope!(let scope, isolate);
        let context = crate::Context::new(scope, Default::default());
        let scope = &mut crate::ContextScope::new(scope, context);
        assert_eq!(cell.with(scope, |_scope, value| *value), 3);
        cell.with(scope, |_scope, value| *value = 4);
        assert_eq!(*cell.get(scope), 4);
    }

    /// A wrapped pointer comes back out of the wrapper it was wrapped on, and
    /// only under the tag it was wrapped with.
    #[test]
    fn a_wrap_round_trips_through_a_js_wrapper() {
        in_context!(scope, {
            let pointer =
                unsafe { make_garbage_collected(scope.get_cpp_heap().expect("a heap"), tracked()) };
            let wrapper = Object::new(scope);
            assert!(!wrapper.is_api_wrapper());
            // SAFETY: the tag is this test's own, and `T` names what was wrapped.
            assert!(unsafe { Object::unwrap::<1, Tracked>(scope, wrapper) }.is_none());

            // SAFETY: as above.
            unsafe { Object::wrap::<1, Tracked>(scope, wrapper, &pointer) };
            assert!(wrapper.is_api_wrapper());
            // SAFETY: as above.
            let back = unsafe { Object::unwrap::<1, Tracked>(scope, wrapper) }.expect("the wrap");
            assert_eq!(back.pointer, pointer.pointer, "the same object, not a copy");
            // A tag the wrapper was not wrapped under sees nothing.
            // SAFETY: as above.
            assert!(unsafe { Object::unwrap::<2, Tracked>(scope, wrapper) }.is_none());

            // The wrap costs the wrapper one property per tag plus a marker, and
            // both are non-enumerable: a program enumerating what it holds does
            // not see the bridge's storage. `getOwnPropertyNames` does — that is
            // the divergence the module header states.
            bind(scope, "wrapper", wrapper.cast::<crate::Value>());
            assert_eq!(eval_number(scope, "Object.keys(wrapper).length"), 0.0);
            assert_eq!(
                eval_number(scope, "Object.getOwnPropertyNames(wrapper).length"),
                2.0
            );
        });
    }

    /// A wrapper already carrying a pointer under a tag is refused rather than
    /// silently overwritten, because the property it carries it at cannot be
    /// replaced.
    #[test]
    fn a_second_wrap_under_one_tag_is_refused() {
        in_context!(scope, {
            let pointer =
                unsafe { make_garbage_collected(scope.get_cpp_heap().expect("a heap"), tracked()) };
            let wrapper = Object::new(scope);
            // SAFETY: the tag is this test's own, and `T` names what was wrapped.
            unsafe {
                Object::wrap::<1, Tracked>(scope, wrapper, &pointer);
                Object::wrap::<1, Tracked>(scope, wrapper, &pointer);
            }

            // The second call reported itself rather than replacing the first:
            // the pending exception is the only channel a `()`-returning call
            // has.
            assert!(scope.engine().has_pending_exception());
            // SAFETY: as above.
            let back = unsafe { Object::unwrap::<1, Tracked>(scope, wrapper) }.expect("the wrap");
            assert_eq!(back.pointer, pointer.pointer);
        });
    }

    /// The isolate owns a heap, and a host that passes one through `CreateParams`
    /// has it become the isolate's.
    #[test]
    fn an_isolate_owns_the_heap_it_was_given() {
        let platform = crate::new_default_platform(0, false).make_shared();
        let heap = Heap::create(platform, HeapCreateParams::default());
        let isolate = crate::Isolate::new(crate::CreateParams::default().cpp_heap(heap));
        assert!(isolate.get_cpp_heap().is_some());

        let isolate = crate::Isolate::new(crate::CreateParams::default());
        assert!(isolate.get_cpp_heap().is_some());
    }

    /// A host's process initialization sequence runs, and the testing knobs a
    /// host's test setup calls are accepted.
    #[test]
    fn the_process_initialization_sequence_is_accepted() {
        let platform = crate::new_default_platform(0, false).make_shared();
        initialize_process(platform);

        let heap = Heap::new();
        let _root = Persistent::new(&unsafe { make_garbage_collected(&heap, tracked()) });
        // SAFETY: the caller's contract is about how the host uses the heap, and
        // this test only checks that the call runs.
        unsafe { heap.collect_garbage_for_testing(EmbedderStackState::NoHeapPointers) };
        heap.enable_detached_garbage_collections_for_testing();

        // SAFETY: the contract is that no heap outlives this, which holds here.
        unsafe { shutdown_process() };
    }
}
