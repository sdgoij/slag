//! The regioned tables a handle's payload lives in: a `Script` handle's text, and
//! every handle's payload cell.
//!
//! A handle has to be `Copy` and one word. A language value, a context and a
//! module are plain `Copy` data, but a script's source is heap data of unbounded
//! size — so a `Script` payload holds a slot index and a generation instead (the
//! pair [`store`] returns and [`entry`] reads back), and this is the table those
//! refer into. Separately, every handle's payload lives in a cell this table
//! allocates ([`cell`]), because a `Local` is a pointer to its payload rather than
//! the payload itself (see `handle`): the representation is what lets a host that
//! stores a raw pointer and rebuilds a handle from it — `Global::into_raw` and
//! the transmutes built on it — work at all.
//!
//! Both are regioned by handle scope. A scope marks the tables when it opens and
//! drops its region when it closes, which is what keeps a runtime that compiles a
//! script per `require`d module from accumulating the text of every one of them,
//! and what keeps a host that makes handles in a loop from accumulating a cell
//! per handle. Because allocation only ever appends, a region covers exactly what
//! was allocated during its scope's life.
//!
//! The generation is what makes a *slot*'s reuse invisible to a handle: a
//! reference whose scope has closed reads `None` rather than the text of a script
//! that later took the slot, and every caller turns that into a panic about its
//! own bug. A cell has no room for a generation — a handle is a pointer and
//! nothing else — so a closed region instead *poisons* its cells with
//! [`Payload::Poisoned`], which every reader panics on. That is a loud bug for a
//! handle that outlived its scope; it is not a substitute for the type system,
//! which is what makes such a handle unbuildable in safe code, and a cell a later
//! region reuses can only be reached by `unsafe`.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;

use crux::heap::GcAny;

use crate::handle::Payload;

struct Slot {
    generation: u32,
    source: Rc<str>,
    /// The script's resource name, as the host's `ScriptOrigin` gave it. It
    /// lives beside the text because it is read at *run* time, where a
    /// dynamic-import refers to it: the engine's parser takes source and no
    /// origin, so the bridge is the only holder of the name.
    name: Option<Rc<str>>,
}

/// What a slot holds: the text and the name a `Script` payload names.
pub(crate) struct Entry {
    pub(crate) source: Rc<str>,
    pub(crate) name: Option<Rc<str>>,
}

/// The engine string for a host-given script name, or `None` when there is none.
pub(crate) fn engine_name(name: Option<&str>) -> Option<crux::string::JsString> {
    name.map(crux::string::JsString::from_utf8)
}

/// Where an open region begins in both tables.
#[derive(Clone, Copy)]
struct Mark {
    slots: usize,
    cells: usize,
}

/// The payload cells handles point into.
///
/// Chunked rather than one `Vec` because a cell's address *is* the handle: a
/// `Vec` that grew would reallocate and move every cell with it, invalidating
/// handles that are still live. A chunk's buffer never moves once allocated, so
/// neither does any cell in it.
#[derive(Default)]
struct Cells {
    chunks: Vec<Vec<Payload>>,
    /// The total number of cells handed out, across every chunk.
    len: usize,
}

/// How many cells a chunk holds. Small enough that a host with many short-lived
/// scopes does not retain a large buffer per thread, large enough that a busy
/// scope does not pay an allocation per few handles.
const CELLS_PER_CHUNK: usize = 256;

impl Cells {
    /// Append `payload` and return the cell holding it.
    fn push(&mut self, payload: Payload) -> NonNull<Payload> {
        let index = self.len;
        let chunk_index = index / CELLS_PER_CHUNK;
        if chunk_index == self.chunks.len() {
            self.chunks.push(Vec::with_capacity(CELLS_PER_CHUNK));
        }
        let chunk = &mut self.chunks[chunk_index];
        let offset = index % CELLS_PER_CHUNK;
        chunk.push(payload);
        self.len += 1;
        NonNull::from(&chunk[offset])
    }

    /// Overwrite every cell from `base` with the poison sentinel, so a handle
    /// whose region this was fails loudly at its next read instead of reading
    /// whatever a later region writes into the same slot.
    fn poison_from(&mut self, base: usize) {
        for index in base..self.len {
            self.chunks[index / CELLS_PER_CHUNK][index % CELLS_PER_CHUNK] = Payload::Poisoned;
        }
    }

    /// Release everything from `base`: poison it, then hand the slots back for
    /// reuse. The chunks' buffers stay allocated — the same bound a scope
    /// already gives `store`'s slots — and the poison stands until a later
    /// allocation overwrites it.
    fn close(&mut self, base: usize) {
        self.poison_from(base);
        let chunk_index = base / CELLS_PER_CHUNK;
        for chunk in self.chunks.iter_mut().skip(chunk_index + 1) {
            chunk.clear();
        }
        if let Some(chunk) = self.chunks.get_mut(chunk_index) {
            chunk.truncate(base % CELLS_PER_CHUNK);
        }
        self.len = base;
    }

    /// Visit the engine box every live cell names, for the collector. The live
    /// cells are exactly `[0, len)`: closing a region lowers `len`, so a handle
    /// whose scope has closed is no longer a root either.
    fn visit(&self, visit: &mut dyn FnMut(GcAny)) {
        for index in 0..self.len {
            self.chunks[index / CELLS_PER_CHUNK][index % CELLS_PER_CHUNK].trace(visit);
        }
    }
}

/// The cell arena, as a source of precise roots.
///
/// A scoped handle is a *pointer* into [`Cells`], so the payload it names is not
/// on the Rust stack for the conservative scan to find — and a `Local` still has
/// to keep its value alive until its scope closes, which is the whole contract a
/// handle scope carries. So the live cells are registered with the collector
/// ([`crux::heap::register_root_source`]), the same way a C surface's handle
/// tables are: the table lives outside the heap, and the engine owns it not.
struct CellsRoots;

impl crux::heap::RootSource for CellsRoots {
    fn roots(&self, visit: &mut dyn FnMut(GcAny)) {
        TABLE.with(|table| table.borrow().cells.visit(visit));
    }
}

static CELLS_ROOTS: CellsRoots = CellsRoots;

thread_local! {
    /// Whether this thread has registered its cell arena. A thread-local because
    /// the arena is: a worker thread's handles root that thread's heap and no
    /// other.
    static ROOTS_REGISTERED: Cell<bool> = const { Cell::new(false) };
}

/// Make this thread's cell arena a root source for the collector, once.
fn register_cell_roots() {
    ROOTS_REGISTERED.with(|registered| {
        if registered.replace(true) {
            return;
        }
        crux::heap::register_root_source(&CELLS_ROOTS);
    });
}

#[derive(Default)]
struct Table {
    slots: Vec<Slot>,
    /// One mark per open handle scope: where its region begins.
    marks: Vec<Mark>,
    cells: Cells,
    /// Handed out with each script. A stale pair cannot name a later script
    /// that reused the slot unless this wraps, which takes 2^32 scripts.
    issued: u32,
}

thread_local! {
    static TABLE: RefCell<Table> = RefCell::new(Table::default());
}

/// Open a region, at the start of a handle scope's life.
pub(crate) fn open_region() {
    register_cell_roots();
    TABLE.with(|table| {
        let mut table = table.borrow_mut();
        let mark = Mark {
            slots: table.slots.len(),
            cells: table.cells.len,
        };
        table.marks.push(mark);
    });
}

/// Close the innermost region, poisoning its cells and dropping its text. Every
/// cell from the region's mark on is released, so a later scope reuses them —
/// the poison stands only until that reuse.
pub(crate) fn close_region() {
    TABLE.with(|table| {
        let mut table = table.borrow_mut();
        let mark = table
            .marks
            .pop()
            .expect("bridge bug: a handle scope closed without having opened");
        table.slots.truncate(mark.slots);
        table.cells.close(mark.cells);
    });
}

/// Put `payload` in the innermost open region, and return the cell holding it:
/// what a [`Local`](crate::Local) is.
pub(crate) fn cell(payload: Payload) -> NonNull<Payload> {
    TABLE.with(|table| {
        let mut table = table.borrow_mut();
        assert!(
            !table.marks.is_empty(),
            "bridge bug: a handle was made with no handle scope open"
        );
        table.cells.push(payload)
    })
}

/// Put `source` in the innermost open region, and return what a handle carries:
/// the slot it went into and that slot's generation.
pub(crate) fn store(source: Rc<str>, name: Option<Rc<str>>) -> (usize, u32) {
    TABLE.with(|table| {
        let mut table = table.borrow_mut();
        assert!(
            !table.marks.is_empty(),
            "bridge bug: a script was compiled with no handle scope open"
        );
        table.issued = table.issued.wrapping_add(1);
        let generation = table.issued;
        table.slots.push(Slot {
            generation,
            source,
            name,
        });
        (table.slots.len() - 1, generation)
    })
}

/// What a slot and generation name, or `None` when the region that held it is
/// gone.
pub(crate) fn entry(slot: usize, generation: u32) -> Option<Entry> {
    TABLE.with(|table| {
        let table = table.borrow();
        let slot = table.slots.get(slot)?;
        (slot.generation == generation).then(|| Entry {
            source: slot.source.clone(),
            name: slot.name.clone(),
        })
    })
}

/// The text a slot and generation name, or `None` when the region that held it
/// is gone.
#[cfg(test)]
fn source(slot: usize, generation: u32) -> Option<Rc<str>> {
    entry(slot, generation).map(|entry| entry.source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::api;

    #[test]
    fn a_slot_carries_the_name_beside_the_text() {
        open_region();
        let (slot, generation) = store(Rc::from("source"), Some(Rc::from("file:///a.js")));
        let read = entry(slot, generation).expect("the region is open");
        assert_eq!(read.source.as_ref(), "source");
        assert_eq!(read.name.as_deref(), Some("file:///a.js"));

        // The name dies with the region exactly as the text does.
        close_region();
        assert!(entry(slot, generation).is_none());
    }

    #[test]
    fn a_closed_region_drops_its_text_and_a_stale_pair_reads_nothing() {
        open_region();
        let (slot, generation) = store(Rc::from("first"), None);
        assert_eq!(source(slot, generation).as_deref(), Some("first"));
        close_region();
        assert!(source(slot, generation).is_none());

        // The next region takes the same slot back, and the pair that named the
        // old occupant stays dead.
        open_region();
        let (reused, new_generation) = store(Rc::from("second"), None);
        assert_eq!(reused, slot);
        assert_ne!(new_generation, generation);
        assert!(source(slot, generation).is_none());
        assert_eq!(source(reused, new_generation).as_deref(), Some("second"));
        close_region();
    }

    #[test]
    fn an_enclosing_region_keeps_what_a_nested_scope_allocated_until_it_closes() {
        open_region();
        let (outer_slot, outer_generation) = store(Rc::from("outer"), None);
        open_region();
        let (inner_slot, inner_generation) = store(Rc::from("inner"), None);
        close_region();
        assert!(source(inner_slot, inner_generation).is_none());
        assert_eq!(
            source(outer_slot, outer_generation).as_deref(),
            Some("outer")
        );

        // An allocation after the nested scope closes lands above the outer
        // mark, so the outer region's release covers it too.
        let (later_slot, later_generation) = store(Rc::from("later"), None);
        assert_eq!(later_slot, inner_slot);
        close_region();
        assert!(source(outer_slot, outer_generation).is_none());
        assert!(source(later_slot, later_generation).is_none());
    }

    #[test]
    fn the_table_does_not_grow_across_scopes() {
        for _ in 0..64 {
            open_region();
            let (slot, generation) = store(Rc::from("each"), None);
            assert_eq!(slot, 0);
            assert!(source(slot, generation).is_some());
            close_region();
        }
        TABLE.with(|table| assert!(table.borrow().slots.is_empty()));
    }

    fn undefined() -> Payload {
        Payload::Value(api::Local::undefined())
    }

    /// The cell's address *is* the handle, so it has to survive every later
    /// allocation — which is what the chunking is for, and what a plain growing
    /// `Vec` would fail.
    #[test]
    fn a_cells_address_survives_further_allocations() {
        open_region();
        let first = cell(undefined());
        for _ in 0..(CELLS_PER_CHUNK * 4) {
            let _ = cell(undefined());
        }
        // SAFETY: the region is open, so the cell is live.
        assert!(matches!(unsafe { first.as_ref() }, Payload::Value(_)));
        close_region();
    }

    /// A handle whose region closed reads the poison rather than whatever the
    /// slot held, and the slot comes back for the next region.
    #[test]
    fn a_closed_region_poisons_the_cells_it_held() {
        open_region();
        let stale = cell(undefined());
        close_region();
        // SAFETY: reading the stale cell is the diagnosable bug the poison
        // exists for.
        assert!(matches!(unsafe { stale.as_ref() }, Payload::Poisoned));

        // The next region takes the address back.
        open_region();
        let fresh = cell(undefined());
        assert_eq!(stale, fresh);
        // SAFETY: this region is open.
        assert!(matches!(unsafe { fresh.as_ref() }, Payload::Value(_)));
        close_region();
    }

    /// A nested close releases its cells to the region below, and an
    /// allocation there overwrites the poison — the reuse the contract allows.
    #[test]
    fn a_nested_close_hands_its_cells_back() {
        open_region();
        open_region();
        let nested = cell(undefined());
        close_region();
        // SAFETY: as above — poison until reuse.
        assert!(matches!(unsafe { nested.as_ref() }, Payload::Poisoned));

        let outer = cell(undefined());
        assert_eq!(nested, outer);
        // SAFETY: the outer region is open.
        assert!(matches!(unsafe { outer.as_ref() }, Payload::Value(_)));
        close_region();
    }
}
