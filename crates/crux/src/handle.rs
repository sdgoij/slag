//! Heap handles for values that live on the JavaScript heap.
//!
//! Phase 1 used `Rc`; the arena-GC milestone (PLAN.md §4.3) replaces it with
//! the GC heap's `Gc<T>` behind this alias, so call sites do not change.

/// A handle to a heap-allocated value (a `Copy` pointer into the GC heap).
pub type Handle<T> = crate::heap::Gc<T>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{GcAny, Trace};

    #[derive(Clone)]
    struct Data(u32);

    impl Trace for Data {
        fn trace(&self, _visit: &mut dyn FnMut(GcAny)) {}
    }

    #[test]
    fn handle_shares_aliased_data() {
        let a: Handle<Data> = Handle::new(Data(42));
        let b = a;
        assert_eq!(a.0, b.0);
        assert!(Handle::ptr_eq(a, b));
    }
}
