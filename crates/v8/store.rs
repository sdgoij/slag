//! Where a `Script` handle's text lives.
//!
//! A handle has to be `Copy`, but a script's source is heap data of unbounded
//! size, so the payload cannot hold it inline the way it holds an engine value
//! or a context. It holds a slot index and a generation instead — the pair
//! [`store`] returns and [`source`] reads back — and this is the table those
//! refer into.
//!
//! Slots are regioned by handle scope. A scope marks the table when it opens
//! and drops its region when it closes, which is what keeps a runtime that
//! compiles a script per `require`d module from accumulating the text of every
//! one of them. Because allocation only ever appends, a region covers exactly
//! the scripts compiled during its scope's life.
//!
//! The generation is what makes the table's reuse invisible to a handle: a
//! reference whose scope has closed reads `None` rather than the text of a
//! script that later took the slot, and every caller turns that into a panic
//! about its own bug.

use std::cell::RefCell;
use std::rc::Rc;

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

#[derive(Default)]
struct Table {
    slots: Vec<Slot>,
    /// One mark per open handle scope: where its region begins.
    marks: Vec<usize>,
    /// Handed out with each script. A stale pair cannot name a later script
    /// that reused the slot unless this wraps, which takes 2^32 scripts.
    issued: u32,
}

thread_local! {
    static TABLE: RefCell<Table> = RefCell::new(Table::default());
}

/// Open a region, at the start of a handle scope's life.
pub(crate) fn open_region() {
    TABLE.with(|table| {
        let mut table = table.borrow_mut();
        let base = table.slots.len();
        table.marks.push(base);
    });
}

/// Close the innermost region, dropping the text it holds.
pub(crate) fn close_region() {
    TABLE.with(|table| {
        let mut table = table.borrow_mut();
        let base = table
            .marks
            .pop()
            .expect("bridge bug: a handle scope closed without having opened");
        table.slots.truncate(base);
    });
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
}
