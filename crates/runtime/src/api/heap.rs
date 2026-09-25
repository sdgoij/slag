//! Heap information for a host (`v8::HeapStatistics`).
//!
//! # What a "heap" is here
//!
//! V8's heap is a generational GC heap with a limit, a reserved size, a
//! committed size and an external-memory account. Slag's is a chunked bump
//! arena that grows on demand: it never reserves beyond what it commits and has
//! no limit, so the two numbers V8 distinguishes as *reserved* and *physical* are
//! the same number here, and there is no limit to report.
//!
//! # The four numbers a host gets, and what each means
//!
//! - `total_heap_size` and `total_physical_size`: the bytes the arena has
//!   committed — every chunk's buffer. Equal by construction, and stated rather
//!   than invented.
//! - `used_heap_size`: the bytes the *live* boxes occupy, counted by the same
//!   arena walk the collector uses (each box's own footprint, not the bytes of
//!   anything it points at). Swept slots are excluded.
//! - `external_memory`: the bytes of the buffers the agent holds —
//!   `ArrayBuffer`/`SharedArrayBuffer`/wasm-memory storage, which lives in
//!   `crates/byteblock` outside the arena and so is not in the two numbers above.
//!   It is summed over the engine's own record of live buffer objects
//!   (`Agent::buffer_data`), skipping a detached buffer, and *excluding* a block
//!   that borrows the host's own memory (`ArrayBuffer::new_backing_store_from_ptr`),
//!   whose bytes were never the engine's. One term per buffer object: a host that
//!   wraps a single block as two buffer objects is counted twice, as V8 counts two
//!   backing stores over one allocation.
//!
//! # What is absent rather than answered
//!
//! The crate we stand in for has fifteen accessors. Four of them are facts about
//! this engine and are reported as such; the rest answer a *documented constant*
//! rather than a varying number, because there is no such number here — and a
//! constant a host can reason about is better than a `0` that looks measured:
//!
//! - `heap_size_limit` and `total_available_size`: this arena has no limit, so
//!   the largest value a host can compare against is reported for the limit, and
//!   the limit less the live bytes for the room left. A host's `limit - used`
//!   arithmetic has space in it, and a host that prints the limit sees a number
//!   no V8 build reports — which is the point.
//! - `malloced_memory` and `peak_malloced_memory`: the bytes obtained outside the
//!   arena are `external_memory`'s (the byte blocks), so that is what the first
//!   answers. No high-water mark is kept, so the second answers the current total
//!   — a lower bound, stated rather than implied.
//! - `total_allocated_bytes`: this arena never returns a chunk, so what it has
//!   committed is everything it has ever allocated. Real, and equal to
//!   `total_heap_size` by construction.
//! - `total_heap_size_executable`: zero, truthfully — no code lives in this heap
//!   (the JIT's code is Cranelift's own allocation).
//! - `does_zap_garbage`: false, truthfully — a swept slot is reused as it is,
//!   with no fill pattern written over it.
//! - `number_of_native_contexts`: the live realms, which the agent counts.
//!   `number_of_detached_contexts`: zero, truthfully — no realm is ever detached
//!   here.
//! - `total_global_handles_size` and `used_global_handles_size`: zero, truthfully
//!   — V8's global handles are an off-heap registry of their own, while this
//!   bridge's persistent handles are ordinary cells in this same arena, already
//!   counted in `used_heap_size`.
//!
//! `.notes/embedding.md` §9 records the tier.

use crux::heap;

use super::Isolate;

/// V8 heap numbers (`v8::HeapStatistics`), the four facts this engine has and the
/// nine constants it can defend; see the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapStatistics {
    /// The bytes the arena has committed.
    pub total_heap_size: usize,
    /// The same bytes, under the name V8 uses for physical memory: an arena that
    /// never over-reserves has one number for both.
    pub total_physical_size: usize,
    /// The bytes the live boxes occupy.
    pub used_heap_size: usize,
    /// The bytes of the buffers the agent holds outside the arena.
    pub external_memory: usize,
    /// Everything the arena has ever allocated: it never returns a chunk, so this
    /// is `total_heap_size`.
    pub total_allocated_bytes: usize,
    /// The bytes of the heap holding executable code: zero, because no code lives
    /// in this heap.
    pub total_heap_size_executable: usize,
    /// The bytes obtained outside the arena, which is `external_memory`'s walk.
    pub malloced_memory: usize,
    /// The peak of that account, as far as it is known: no high-water mark is
    /// kept, so this is the current total.
    pub peak_malloced_memory: usize,
    /// The largest heap size the machine allows, because this arena has no
    /// limit.
    pub heap_size_limit: usize,
    /// The room left, which is the limit less the live bytes.
    pub total_available_size: usize,
    /// The live realms.
    pub number_of_native_contexts: usize,
    /// Detached realms: zero, because no realm is ever detached here.
    pub number_of_detached_contexts: usize,
    /// Whether the collector writes a pattern over freed memory: false.
    pub does_zap_garbage: bool,
    /// The bytes of V8's off-heap global-handle registry: zero here, because this
    /// bridge's persistent handles are cells in this arena.
    pub total_global_handles_size: usize,
    /// The used part of that registry: zero, for the same reason.
    pub used_global_handles_size: usize,
}

impl Isolate {
    /// Read the heap's numbers (v8::Isolate::GetHeapStatistics).
    ///
    /// The two arena numbers come from the heap itself (a walk for the live
    /// bytes, which is why a host asks this as a diagnostic rather than in a
    /// loop), and the external bytes from the agent's own record of live buffer
    /// objects.
    pub fn heap_statistics(&mut self) -> HeapStatistics {
        let (committed, live) = heap::with_heap(|heap| (heap.committed_bytes(), heap.live_bytes()));
        // The walk is floored at zero, so the cast is exact.
        let external = external_memory(self) as usize;
        HeapStatistics {
            total_heap_size: committed,
            total_physical_size: committed,
            used_heap_size: live,
            external_memory: external,
            total_allocated_bytes: committed,
            total_heap_size_executable: 0,
            malloced_memory: external,
            peak_malloced_memory: external,
            heap_size_limit: usize::MAX,
            total_available_size: usize::MAX - live,
            number_of_native_contexts: self.agent.realm_count.get(),
            number_of_detached_contexts: 0,
            does_zap_garbage: false,
            total_global_handles_size: 0,
            used_global_handles_size: 0,
        }
    }

    /// Add `change` bytes to the host's external-memory account and answer the
    /// new total (v8::Isolate::AdjustAmountOfExternalAllocatedMemory).
    ///
    /// The one lever a host has to tell the engine about memory it holds outside
    /// the heap — a GPU device's backing, a natively allocated buffer — which the
    /// engine's own walk cannot see. The answer is what
    /// [`HeapStatistics::external_memory`] reports, so a host that records and a
    /// host that asks agree.
    ///
    /// Unlike V8's account, this one does not drive a collection: V8 feeds it to
    /// its GC heuristics, while this engine's collections are triggered by its own
    /// growth policy. The number is reported rather than consulted, which is the
    /// divergence `.notes/embedding.md` §9 records.
    pub fn adjust_external_memory(&mut self, change: i64) -> i64 {
        self.external_memory_adjustment = self.external_memory_adjustment.saturating_add(change);
        external_memory(self)
    }
}

/// The bytes of the buffers this agent holds, plus the host's own account.
///
/// Summed over the agent's own record of live buffer objects, one term per
/// object, with a detached buffer skipped (it has no bytes) and a *borrowed*
/// block excluded (the host's memory, not this engine's). A host that wraps one
/// block as two buffer objects is therefore counted twice, which is what V8 does
/// with two backing stores over one allocation too.
///
/// The host's own account — [`Isolate::adjust_external_memory`] — is added to
/// that walk, because the point of a host adjusting it is precisely that the
/// bytes are outside anything this engine can see. The total floors at zero, so a
/// host that over-corrects with a negative change is told it holds no external
/// memory rather than a negative number of bytes.
fn external_memory(isolate: &Isolate) -> i64 {
    let mut total = isolate.external_memory_adjustment;
    for state in isolate.agent.buffer_data.values() {
        let state = state.borrow();
        if state.detached || state.shared.is_borrowed() {
            continue;
        }
        total = total.saturating_add(state.shared.byte_length() as i64);
    }
    total.max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Context;

    /// A fresh isolate's numbers describe its own arena: committed bytes at least
    /// the live bytes, and no external memory until a host makes a buffer.
    #[test]
    fn the_arena_has_a_size_and_no_external_memory_at_first() {
        let mut isolate = Isolate::new();
        let _context = Context::new(&mut isolate).expect("context");
        let statistics = isolate.heap_statistics();
        assert!(statistics.total_heap_size > 0, "the arena has committed");
        assert!(
            statistics.used_heap_size <= statistics.total_heap_size,
            "live bytes are inside the committed bytes"
        );
        assert_eq!(statistics.external_memory, 0);
        assert_eq!(statistics.total_heap_size, statistics.total_physical_size);
    }

    /// A buffer the host allocates shows up as external memory — the arena's own
    /// numbers cannot see it, because its bytes live outside the arena — and a
    /// *view* over that buffer does not add a term of its own, because a view is
    /// not a buffer object.
    #[test]
    fn a_host_buffer_is_external_memory_but_its_view_is_not() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");
        let before = isolate.heap_statistics();

        context
            .try_eval("var view = new Uint8Array(4096); var again = new Uint8Array(view.buffer);")
            .expect("eval");
        let after = isolate.heap_statistics();
        assert_eq!(
            after.external_memory,
            before.external_memory + 4096,
            "the buffer's bytes are external, and a view over it adds nothing"
        );
    }

    /// A host's own account is added to the buffer walk with a floor at zero, and
    /// what `adjust_external_memory` answers is what the statistic reports.
    #[test]
    fn the_hosts_account_is_added_to_the_statistic_and_floors_at_zero() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");

        assert_eq!(isolate.adjust_external_memory(1 << 20), 1 << 20);
        assert_eq!(isolate.heap_statistics().external_memory, 1 << 20);

        // A buffer's bytes and the host's account are one total.
        context.try_eval("new Uint8Array(4096)").expect("eval");
        assert_eq!(isolate.heap_statistics().external_memory, (1 << 20) + 4096);

        // Over-correcting floors at zero rather than reporting negative bytes.
        assert_eq!(isolate.adjust_external_memory(-(2 << 20)), 0);
        assert_eq!(isolate.heap_statistics().external_memory, 0);
    }
}
