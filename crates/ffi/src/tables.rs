//! Strongly-owned handle tables: C opaque refs are ids into thread-local
//! tables, so values stay alive for as long as the host holds the ref.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use crux::heap::Trace;
use crux::string::JsString;
use crux::value::Value;

/// A table of strongly-owned entries with stable ids (0 is reserved).
pub struct Table<T> {
    next: Cell<u64>,
    entries: RefCell<HashMap<u64, T>>,
    /// Reference counts, so `Retain`/`Release` pairs behave like JSC's.
    counts: RefCell<HashMap<u64, usize>>,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self {
            next: Cell::new(1),
            entries: RefCell::new(HashMap::new()),
            counts: RefCell::new(HashMap::new()),
        }
    }
}

impl<T> Table<T> {
    /// Store `value`, returning its stable id.
    pub fn insert(&self, value: T) -> u64 {
        let id = self.next.get();
        self.next.set(id + 1);
        self.entries.borrow_mut().insert(id, value);
        self.counts.borrow_mut().insert(id, 1);
        id
    }

    /// Add one reference to a live entry; false when the id is not live.
    pub fn retain(&self, id: u64) -> bool {
        if !self.entries.borrow().contains_key(&id) {
            return false;
        }
        if let Some(count) = self.counts.borrow_mut().get_mut(&id) {
            *count += 1;
        }
        true
    }

    /// Drop one reference; the entry is removed with its last reference.
    pub fn release(&self, id: u64) {
        let last = {
            let mut counts = self.counts.borrow_mut();
            match counts.get_mut(&id) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                    false
                }
                Some(_) => {
                    counts.remove(&id);
                    true
                }
                None => false,
            }
        };
        if last {
            self.entries.borrow_mut().remove(&id);
        }
    }

    /// The stored value, if the id is live.
    pub fn get(&self, id: u64) -> Option<T>
    where
        T: Clone,
    {
        self.entries.borrow().get(&id).cloned()
    }

    /// Whether the id is live.
    pub fn contains(&self, id: u64) -> bool {
        self.entries.borrow().contains_key(&id)
    }

    /// Drop the stored value unconditionally.
    pub fn remove(&self, id: u64) {
        self.entries.borrow_mut().remove(&id);
        self.counts.borrow_mut().remove(&id);
    }
}

impl<T: Trace> Table<T> {
    /// Report the boxes the entries reach, as roots for the collector.
    ///
    /// An entry is a `Value` or a `JsString`, each of which already knows its
    /// own edges, so a table's roots are exactly its entries'.
    fn gc_roots(&self, visit: &mut dyn FnMut(crux::heap::GcAny)) {
        for entry in self.entries.borrow().values() {
            entry.trace(visit);
        }
    }
}

/// A string kept in its own allocation, so `JSStringGetCharactersPtr` can
/// hand out a pointer that survives table rehashing and insertion.
struct StableString(Box<JsString>);

impl Clone for StableString {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Trace for StableString {
    fn trace(&self, visit: &mut dyn FnMut(crux::heap::GcAny)) {
        self.0.trace(visit);
    }
}

thread_local! {
    /// JS values held by the host (JSValueRef).
    static VALUE_TABLE: Table<Value> = Table::default();
    /// JS strings held by the host (JSStringRef).
    static STRING_TABLE: Table<StableString> = Table::default();
    /// Whether this thread has registered its tables with the collector. A
    /// thread-local because the tables are: a worker thread's refs root that
    /// thread's heap and no other.
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
}

/// Both tables, as one root source.
///
/// A `static` so its address is stable: registration is idempotent by address.
struct HandleTables;

impl crux::heap::RootSource for HandleTables {
    fn roots(&self, visit: &mut dyn FnMut(crux::heap::GcAny)) {
        VALUE_TABLE.with(|table| table.gc_roots(visit));
        STRING_TABLE.with(|table| table.gc_roots(visit));
    }
}

static HANDLE_TABLES: HandleTables = HandleTables;

/// Retain a value, returning its host-visible id.
pub fn retain_value(value: Value) -> u64 {
    register_tables();
    VALUE_TABLE.with(|table| table.insert(value))
}

/// The retained value for `id`, if live.
pub fn value(id: u64) -> Option<Value> {
    VALUE_TABLE.with(|table| table.get(id))
}

/// Release a retained value.
pub fn release_value(id: u64) {
    VALUE_TABLE.with(|table| table.release(id));
}

/// Add a reference to a live value ref, for `JSValueProtect`.
pub fn retain_value_ref(id: u64) -> bool {
    VALUE_TABLE.with(|table| table.retain(id))
}

/// Retain a string, returning its host-visible id.
pub fn retain_string(string: JsString) -> u64 {
    register_tables();
    STRING_TABLE.with(|table| table.insert(StableString(Box::new(string))))
}

/// Add a reference to a live string ref, for `JSStringRetain`.
pub fn retain_string_ref(id: u64) -> bool {
    STRING_TABLE.with(|table| table.retain(id))
}

/// The retained string for `id`, if live.
pub fn string(id: u64) -> Option<JsString> {
    STRING_TABLE.with(|table| table.get(id).map(|entry| (*entry.0).clone()))
}

/// Run `f` with the retained string for `id`, without cloning it. The
/// pointer returned from such a borrow stays valid while the ref is
/// retained: the string lives in its own `Box`, which a table rehash only
/// moves as a pointer.
pub fn with_string<R>(id: u64, f: impl FnOnce(&JsString) -> R) -> Option<R> {
    STRING_TABLE.with(|table| {
        let entries = table.entries.borrow();
        entries.get(&id).map(|entry| f(&entry.0))
    })
}

/// Release a retained string.
pub fn release_string(id: u64) {
    STRING_TABLE.with(|table| table.release(id));
}

/// Make this thread's tables roots for the collector, once.
///
/// A ref a host holds has to keep its value alive — that is the whole of what a
/// `JSValueRef` promises (`crates/jsc/src/lib.rs`) — and the collector cannot see
/// a thread-local `HashMap`, so the tables are registered as a root source at
/// the first retention on this thread.
fn register_tables() {
    REGISTERED.with(|registered| {
        if registered.replace(true) {
            return;
        }
        crux::heap::register_root_source(&HANDLE_TABLES);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_through_the_table() {
        let id = retain_value(Value::Number(42.0));
        assert!(id != 0);
        assert_eq!(value(id), Some(Value::Number(42.0)));
        release_value(id);
        assert_eq!(value(id), None);
    }

    #[test]
    fn ids_are_distinct() {
        let a = retain_value(Value::Undefined);
        let b = retain_value(Value::Null);
        assert_ne!(a, b);
        release_value(a);
        release_value(b);
    }

    #[test]
    fn refcounts_keep_an_entry_until_the_last_release() {
        let id = retain_value(Value::Number(1.0));
        assert!(retain_value_ref(id));
        release_value(id);
        assert_eq!(value(id), Some(Value::Number(1.0)));
        release_value(id);
        assert_eq!(value(id), None);
    }

    #[test]
    fn a_string_pointer_survives_table_growth() {
        let id = retain_string(JsString::from_utf8("abc"));
        let pointer = with_string(id, |string| string.as_slice().as_ptr()).unwrap();
        for index in 0..2000 {
            let extra = retain_string(JsString::from_utf8(&format!("filler-{index}")));
            release_string(extra);
        }
        assert_eq!(
            with_string(id, |string| string.as_slice().as_ptr()),
            Some(pointer),
            "the characters pointer moved when the table grew"
        );
        release_string(id);
    }

    #[test]
    fn strings_round_trip_through_the_table() {
        let id = retain_string(JsString::from_utf8("hi"));
        assert_eq!(string(id), Some(JsString::from_utf8("hi")));
        release_string(id);
        assert_eq!(string(id), None);
    }

    /// A value the host retains through this table is a root, which is the
    /// promise a `JSValueRef` makes: the value a host holds never dangles
    /// (`crates/jsc/src/lib.rs`). The table is a thread-local `HashMap` — exactly
    /// the off-stack buffer the conservative scan cannot see — and
    /// `Heap::collect` is the deterministic entry point: it does not scan, so
    /// only a registered root keeps the box.
    #[test]
    fn a_retained_value_survives_a_collection() {
        use crux::handle::Handle;

        let handle = Handle::new(JsString::from_utf8("retained"));
        let id = retain_value(Value::String(handle));
        let box_addr = handle.as_any().addr();

        let swept = crux::heap::with_heap_mut(|heap| heap.collect(&[]));
        assert!(
            !swept.contains(&box_addr),
            "a value the host still holds was swept: {swept:?}"
        );
        assert_eq!(
            value(id).and_then(|held| held.as_string().map(|text| text.to_string_lossy())),
            Some("retained".to_string()),
            "the ref no longer names the value it retained"
        );
    }

    /// The same for the string table, whose contents a `JSStringRef` names —
    /// and it is about a different thing than the value half: a rope's parts
    /// are **boxes**, and the table is their only reference, so a `JsString`
    /// entry is what makes the string half load-bearing rather than a copy.
    #[test]
    fn a_retained_string_survives_a_collection() {
        use crux::handle::Handle;

        let left = Handle::new(JsString::from_utf8(&"a".repeat(2000)));
        let right = Handle::new(JsString::from_utf8(&"b".repeat(2000)));
        let rope = JsString::concat(&left, &right);
        let id = retain_string(JsString::owned_of(&rope));
        let (left_box, right_box) = (left.as_any().addr(), right.as_any().addr());

        let swept = crux::heap::with_heap_mut(|heap| heap.collect(&[]));
        assert!(
            !swept.contains(&left_box) && !swept.contains(&right_box),
            "a rope's parts are reachable only through the table: {swept:?}"
        );
        assert_eq!(
            string(id).map(|text| text.to_string_lossy().len()),
            Some(4000),
            "the retained string still reads"
        );
        // The rope itself is the table's entry; the handles above are its parts.
        let _ = rope;
    }
}
