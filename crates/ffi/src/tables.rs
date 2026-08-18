//! Strongly-owned handle tables: C opaque refs are ids into thread-local
//! tables, so values stay alive for as long as the host holds the ref.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use crux::string::JsString;
use crux::value::Value;

/// A table of strongly-owned entries with stable ids (0 is reserved).
pub struct Table<T> {
    next: Cell<u64>,
    entries: RefCell<HashMap<u64, T>>,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self {
            next: Cell::new(1),
            entries: RefCell::new(HashMap::new()),
        }
    }
}

impl<T> Table<T> {
    /// Store `value`, returning its stable id.
    pub fn insert(&self, value: T) -> u64 {
        let id = self.next.get();
        self.next.set(id + 1);
        self.entries.borrow_mut().insert(id, value);
        id
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

    /// Drop the stored value.
    pub fn remove(&self, id: u64) {
        self.entries.borrow_mut().remove(&id);
    }
}

thread_local! {
    /// JS values held by the host (JSValueRef).
    static VALUE_TABLE: Table<Value> = Table::default();
    /// JS strings held by the host (JSStringRef).
    static STRING_TABLE: Table<JsString> = Table::default();
}

/// Retain a value, returning its host-visible id.
pub fn retain_value(value: Value) -> u64 {
    VALUE_TABLE.with(|table| table.insert(value))
}

/// The retained value for `id`, if live.
pub fn value(id: u64) -> Option<Value> {
    VALUE_TABLE.with(|table| table.get(id))
}

/// Release a retained value.
pub fn release_value(id: u64) {
    VALUE_TABLE.with(|table| table.remove(id));
}

/// Retain a string, returning its host-visible id.
pub fn retain_string(string: JsString) -> u64 {
    STRING_TABLE.with(|table| table.insert(string))
}

/// The retained string for `id`, if live.
pub fn string(id: u64) -> Option<JsString> {
    STRING_TABLE.with(|table| table.get(id))
}

/// Release a retained string.
pub fn release_string(id: u64) {
    STRING_TABLE.with(|table| table.remove(id));
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
    fn strings_round_trip_through_the_table() {
        let id = retain_string(JsString::from_utf8("hi"));
        assert_eq!(string(id), Some(JsString::from_utf8("hi")));
        release_string(id);
        assert_eq!(string(id), None);
    }
}
