//! Heap information and the host's collection callbacks (`v8::HeapStatistics`,
//! `v8::GCType`, `v8::AddGCPrologueCallback` and its neighbours).
//!
//! Four numbers, and each one is the engine's own: the arena's committed bytes
//! (reported as both the heap's size and its physical size, because an arena that
//! never over-reserves has one number for both), the bytes its live boxes occupy,
//! and the bytes of the buffers the agent holds outside the arena. What a host
//! *cannot* read is the rest of the crate's fifteen accessors — no heap limit, no
//! allocation total, no handle-registry sizes, no executable bytes — and they are
//! absent rather than answered with a plausible zero; `runtime::api::heap`'s
//! module documentation is where each is accounted for, and `.notes/embedding.md`
//! §9 records the tier.
//!
//! The struct and its accessors are the crate's shape — a value with methods, not
//! public fields — so a host that keeps one and reads it later reads the numbers
//! it was given rather than asking again.
//!
//! The callbacks are the engine's own collection boundary reached through this
//! bridge: the engine runs a collection at known entry points (`runtime`'s
//! `Agent::collect_with`, which every host-triggered and every allocation-triggered
//! collection goes through) and an observer registered there is what carries a
//! host's prologue and epilogue callbacks. The engine's two generations are the
//! only kinds that fire — there is no separate incremental or weak-callback pass
//! to report — and the heap has no spaces, which is why
//! [`Isolate::get_heap_space_statistics`] answers nothing.

use std::ffi::{CStr, CString, c_void};

use crux::heap::{GcGeneration, GcObserver};
use runtime::api;

use crate::isolate::Isolate;

/// V8 heap numbers (`v8::HeapStatistics`).
#[derive(Debug, Clone, Copy)]
pub struct HeapStatistics(pub(crate) api::HeapStatistics);

impl HeapStatistics {
    /// The size of the heap (v8::HeapStatistics::total_heap_size): the bytes the
    /// arena has committed.
    pub fn total_heap_size(&self) -> usize {
        self.0.total_heap_size
    }

    /// The physical memory the heap holds
    /// (v8::HeapStatistics::total_physical_size): the same bytes here, because
    /// the arena commits exactly what it uses.
    pub fn total_physical_size(&self) -> usize {
        self.0.total_physical_size
    }

    /// The bytes the heap's live objects occupy
    /// (v8::HeapStatistics::used_heap_size), counted by the arena's own walk: each
    /// box's footprint, without the bytes of anything it points at.
    pub fn used_heap_size(&self) -> usize {
        self.0.used_heap_size
    }

    /// The bytes of the buffers the isolate holds outside the heap
    /// (v8::HeapStatistics::external_memory): `ArrayBuffer`,
    /// `SharedArrayBuffer` and wasm-memory storage.
    pub fn external_memory(&self) -> usize {
        self.0.external_memory
    }
}

impl Isolate {
    /// Read the heap's numbers (v8::Isolate::GetHeapStatistics).
    ///
    /// The live bytes come from an arena walk, so this is a diagnostic rather
    /// than something to call in a loop.
    pub fn get_heap_statistics(&mut self) -> HeapStatistics {
        HeapStatistics(self.engine_mut().heap_statistics())
    }
}

/// What kind of collection a callback is being told about, and what a host
/// installs one to watch (`v8::GCType`).
///
/// The values are V8's, because a host passes one of them back as the *filter* it
/// installs a callback with and V8 tests it as the bitmask it is, so the numbers
/// are observable even though the variants are an enum. One is this engine's per
/// collection: a young-only collection reports
/// [`kGCTypeScavenge`](GCType::kGCTypeScavenge) and a whole-heap one
/// [`kGCTypeMarkSweepCompact`](GCType::kGCTypeMarkSweepCompact); the phases this
/// engine has no separate pass for never fire.
#[repr(C)]
// The variant names are V8's, and a host's code reads them by these names.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GCType {
    kGCTypeScavenge = 1,
    kGCTypeMarkSweepCompact = 2,
    kGCTypeIncrementalMarking = 4,
    kGCTypeProcessWeakCallbacks = 8,
    kGCTypeMinorMarkCompact = 16,
    kGCTypeMinorMarkSweep = 32,
    /// Every kind this engine reports, which is what a host that wants all of
    /// them filters on. V8's own value: the minor kinds are deliberately not in
    /// it, there as here.
    kGCTypeAll = 15,
}

impl GCType {
    /// Whether a callback installed with this filter wants a collection of
    /// `actual` — V8's own test, on the bits rather than the variants.
    fn wants(self, actual: GCType) -> bool {
        (self as u32) & (actual as u32) != 0
    }
}

bitflags::bitflags! {
    /// What a collection carries beyond its kind (`v8::GCCallbackFlags`).
    ///
    /// This engine's collections carry none of these, so a callback always sees
    /// [`kNoGCCallbackFlags`](Self::kNoGCCallbackFlags): the flags say what the
    /// collector was *asked* for, and nothing asks this one.
    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct GCCallbackFlags: u32 {
        const kNoGCCallbackFlags = 0;
        const kGCCallbackFlagConstructRetainedObjectInfos = 1 << 1;
        const kGCCallbackFlagForced = 1 << 2;
        const kGCCallbackFlagSynchronousPhantomCallbackProcessing = 1 << 3;
        const kGCCallbackFlagCollectAllAvailableGarbage = 1 << 4;
        const kGCCallbackFlagCollectAllExternalMemory = 1 << 5;
    }
}

/// A host's collection callback (`v8::GCCallback`).
///
/// Handed the isolate the collection is running for, the kind it is, the flags
/// it carries and the data the host installed the callback with.
pub type GcCallback = extern "C" fn(
    isolate: crate::UnsafeRawIsolatePtr,
    gc_type: GCType,
    flags: GCCallbackFlags,
    data: *mut c_void,
);

/// Which end of a collection a callback runs at.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GcPhase {
    Prologue,
    Epilogue,
}

/// One host callback with what it was installed for.
pub(crate) struct GcCallbackEntry {
    pub(crate) phase: GcPhase,
    pub(crate) callback: GcCallback,
    pub(crate) data: *mut c_void,
    pub(crate) filter: GCType,
}

/// The engine observer that runs this isolate's collection callbacks.
///
/// One per isolate, registered the first time a host installs a callback. The
/// callbacks themselves are read from the isolate when a collection happens, so
/// one installed later runs without the observer being registered again.
struct IsolateGcObserver {
    isolate: Isolate,
}

impl GcObserver for IsolateGcObserver {
    fn prologue(&self, generation: GcGeneration) {
        self.run(GcPhase::Prologue, gc_type_of(generation));
    }

    fn epilogue(&self, generation: GcGeneration) {
        self.run(GcPhase::Epilogue, gc_type_of(generation));
    }
}

impl IsolateGcObserver {
    fn run(&self, phase: GcPhase, gc_type: GCType) {
        // Copied out before the host runs: a callback may install or drop
        // another, and the isolate's list must not be borrowed across host code.
        let callbacks: Vec<(GcCallback, *mut c_void)> = {
            let inner = self.isolate.inner();
            let callbacks = inner.gc_callbacks.borrow();
            callbacks
                .iter()
                .filter(|entry| entry.phase == phase && entry.filter.wants(gc_type))
                .map(|entry| (entry.callback, entry.data))
                .collect()
        };
        for (callback, data) in callbacks {
            callback(
                crate::UnsafeRawIsolatePtr::from_inner_ptr(self.isolate.as_inner_ptr()),
                gc_type,
                GCCallbackFlags::kNoGCCallbackFlags,
                data,
            );
        }
    }
}

/// The kind a generation reports as (see [`GCType`]).
fn gc_type_of(generation: GcGeneration) -> GCType {
    match generation {
        GcGeneration::Minor => GCType::kGCTypeScavenge,
        GcGeneration::Major => GCType::kGCTypeMarkSweepCompact,
    }
}

impl Isolate {
    /// Install a callback for the start of a collection
    /// (`v8::Isolate::AddGCPrologueCallback`).
    ///
    /// `gc_type` is the filter: the callback runs for a collection whose kind is
    /// one of its bits, exactly as there, and the engine's two generations are
    /// the only kinds that ever fire.
    pub fn add_gc_prologue_callback(
        &mut self,
        callback: GcCallback,
        data: *mut c_void,
        gc_type: GCType,
    ) {
        self.install_gc_callback(GcPhase::Prologue, callback, data, gc_type);
    }

    /// Install a callback for the end of a collection
    /// (`v8::Isolate::AddGCEpilogueCallback`).
    pub fn add_gc_epilogue_callback(
        &mut self,
        callback: GcCallback,
        data: *mut c_void,
        gc_type: GCType,
    ) {
        self.install_gc_callback(GcPhase::Epilogue, callback, data, gc_type);
    }

    fn install_gc_callback(
        &mut self,
        phase: GcPhase,
        callback: GcCallback,
        data: *mut c_void,
        gc_type: GCType,
    ) {
        self.inner()
            .gc_callbacks
            .borrow_mut()
            .push(GcCallbackEntry {
                phase,
                callback,
                data,
                filter: gc_type,
            });
        if self.inner().gc_observer_registered.replace(true) {
            return;
        }
        let isolate = *self;
        let agent = self.inner().engine.agent_ptr();
        // SAFETY: `agent_ptr` is this isolate's own agent — the two are the same
        // address, which `IsolateInner`'s layout asserts — and the handle is a
        // pointer whose isolate outlives every collection it drives, so the
        // observer's copy is live when the engine runs it.
        unsafe { (*agent).add_collection_observer(Box::new(IsolateGcObserver { isolate })) };
    }

    /// The number of embedder data slots the isolate has
    /// (`v8::Isolate::GetNumberOfDataSlots`).
    ///
    /// Zero, and truthfully: V8's are an *indexed* array the embedder asks for
    /// room in, while this bridge's [`get_slot`](Self::get_slot) is keyed by
    /// type and has no index space at all, so there is no count to report.
    pub fn get_number_of_data_slots(&self) -> u32 {
        0
    }

    /// The heap space at `index`, if there is one
    /// (`v8::Isolate::GetHeapSpaceStatistics`).
    ///
    /// `None` at every index: V8's heap is a set of spaces — a new space, an old
    /// space, code and map spaces — and this engine's is one chunked arena with
    /// no such division. The whole-heap numbers are
    /// [`get_heap_statistics`](Self::get_heap_statistics)'s, so a host loses no
    /// measurement here; what it does not get is a per-space breakdown that does
    /// not exist to give.
    pub fn get_heap_space_statistics(&self, _index: usize) -> Option<HeapSpaceStatistics> {
        None
    }
}

/// One of a heap's spaces (`v8::HeapSpaceStatistics`).
///
/// The crate reports one of these per V8 heap space; this engine has no spaces,
/// so [`Isolate::get_heap_space_statistics`] never answers one. The type is the
/// crate's shape and its accessors are what a host's per-space code reads once it
/// has one, so they answer what the record holds rather than nothing.
pub struct HeapSpaceStatistics {
    name: CString,
    size: usize,
    used: usize,
    available: usize,
    physical: usize,
}

impl HeapSpaceStatistics {
    /// A record, for the shape's test.
    ///
    /// Nothing in this engine produces one — there are no spaces — so the only
    /// caller is the test that pins what a host would read from it.
    #[cfg(test)]
    pub(crate) fn new(
        name: &str,
        size: usize,
        used: usize,
        available: usize,
        physical: usize,
    ) -> Self {
        Self {
            name: CString::new(name).expect("a space name has no interior nul"),
            size,
            used,
            available,
            physical,
        }
    }

    /// The space's name (`v8::HeapSpaceStatistics::space_name`).
    ///
    /// A `&CStr` rather than V8's `*const c_char`: the sites in this tree wrap a
    /// raw pointer back into a `CStr` themselves, so a pointer into this record's
    /// own storage would be the same string with no safety gained.
    pub fn space_name(&self) -> &CStr {
        &self.name
    }

    /// The space's size in bytes (`v8::HeapSpaceStatistics::space_size`).
    pub fn space_size(&self) -> usize {
        self.size
    }

    /// The bytes the space's live objects occupy
    /// (`v8::HeapSpaceStatistics::space_used_size`).
    pub fn space_used_size(&self) -> usize {
        self.used
    }

    /// The room the space has left
    /// (`v8::HeapSpaceStatistics::space_available_size`).
    pub fn space_available_size(&self) -> usize {
        self.available
    }

    /// The bytes the space has committed
    /// (`v8::HeapSpaceStatistics::physical_space_size`).
    pub fn physical_space_size(&self) -> usize {
        self.physical
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{eval, in_context};

    /// A collection callback is handed the isolate it was installed on, the
    /// collection's kind, the flags and the host's data — and the filter it was
    /// installed with decides whether it runs at all.
    #[test]
    fn a_gc_callback_brackets_a_collection() {
        use std::cell::Cell;

        use crate::UnsafeRawIsolatePtr;

        /// What the callback checks its isolate against: only the isolate it was
        /// installed on has one of these.
        struct Marker;

        extern "C" fn count(
            isolate: UnsafeRawIsolatePtr,
            gc_type: GCType,
            flags: GCCallbackFlags,
            data: *mut c_void,
        ) {
            // SAFETY: the host installed this callback on a live isolate and
            // keeps the `Cell` the data pointer names alive for the collection.
            let back = unsafe { Isolate::from_raw_isolate_ptr_unchecked(isolate) };
            assert!(
                back.get_slot::<Marker>().is_some(),
                "the callback is handed the isolate it was installed on"
            );
            assert_eq!(
                gc_type,
                GCType::kGCTypeMarkSweepCompact,
                "a whole-heap collection"
            );
            assert_eq!(flags, GCCallbackFlags::kNoGCCallbackFlags);
            // SAFETY: as above.
            let counter = unsafe { &*data.cast::<Cell<usize>>() };
            counter.set(counter.get() + 1);
        }

        in_context!(scope, {
            scope.set_slot(Marker);
            let prologues = Cell::new(0usize);
            let epilogues = Cell::new(0usize);
            let young_only = Cell::new(0usize);
            scope.add_gc_prologue_callback(
                count,
                &prologues as *const Cell<usize> as *mut c_void,
                GCType::kGCTypeAll,
            );
            scope.add_gc_epilogue_callback(
                count,
                &epilogues as *const Cell<usize> as *mut c_void,
                GCType::kGCTypeAll,
            );
            // Filtered to the young generation, which this collection is not.
            scope.add_gc_prologue_callback(
                count,
                &young_only as *const Cell<usize> as *mut c_void,
                GCType::kGCTypeScavenge,
            );

            crate::realm_of(scope).with_agent(|agent| agent.collect_garbage());

            assert_eq!(prologues.get(), 1, "the prologue ran once, before the mark");
            assert_eq!(epilogues.get(), 1, "the epilogue ran once, after the sweep");
            assert_eq!(
                young_only.get(),
                0,
                "a callback filtered to a kind this collection is not does not run"
            );
        });
    }

    /// The engine has no heap spaces and no indexed data slots, so both answers
    /// are empty rather than fabricated; the whole-heap numbers are
    /// [`Isolate::get_heap_statistics`]'s.
    #[test]
    fn the_engine_reports_no_heap_spaces_and_no_data_slots() {
        in_context!(scope, {
            assert_eq!(scope.get_number_of_data_slots(), 0);
            assert!(
                scope.get_heap_space_statistics(0).is_none(),
                "one chunked arena is not a set of spaces"
            );
            assert!(scope.get_heap_space_statistics(7).is_none());
        });
    }

    /// The space record is the crate's shape: what a host reads from one is what
    /// it holds. Nothing in this engine produces one — the heap has no spaces —
    /// so the shape is pinned here rather than reached through the API.
    #[test]
    fn a_heap_space_record_answers_what_it_holds() {
        let space = HeapSpaceStatistics::new("arena", 4096, 2048, 0, 4096);
        assert_eq!(space.space_name().to_str().unwrap(), "arena");
        assert_eq!(space.space_size(), 4096);
        assert_eq!(space.space_used_size(), 2048);
        assert_eq!(space.space_available_size(), 0);
        assert_eq!(space.physical_space_size(), 4096);
    }

    /// The shape `deno_core`'s `op_memory_usage` reads, from the receiver it uses:
    /// a scope. The answers are the engine's own, and a buffer a host allocates is
    /// the only thing that moves between the two reads — the arena's numbers
    /// cannot see its bytes.
    #[test]
    fn a_host_reads_the_engines_own_heap_numbers() {
        in_context!(scope, {
            let statistics = scope.get_heap_statistics();
            assert!(statistics.total_heap_size() > 0, "the arena has committed");
            assert!(
                statistics.used_heap_size() <= statistics.total_heap_size(),
                "live bytes are inside the committed bytes"
            );
            assert_eq!(
                statistics.total_heap_size(),
                statistics.total_physical_size(),
                "an arena that never over-reserves has one number for both"
            );

            let before = statistics.external_memory();
            eval(scope, "var view = new Uint8Array(4096);");
            assert_eq!(
                scope.get_heap_statistics().external_memory(),
                before + 4096,
                "a buffer's bytes are external to the arena"
            );
        });
    }
}
