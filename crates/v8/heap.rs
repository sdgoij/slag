//! Heap information (`v8::HeapStatistics`).
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

#[cfg(test)]
mod tests {
    use crate::test_support::{eval, in_context};

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
