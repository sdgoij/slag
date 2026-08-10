//! Heap handles for values that live on the JavaScript heap.
//!
//! Phase 1 uses `Rc`; the arena-GC milestone (PLAN.md §4.3) replaces it behind
//! this alias so call sites do not change.

/// A handle to a heap-allocated value.
pub type Handle<T> = std::rc::Rc<T>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_shares_aliased_data() {
        let a: Handle<u32> = Handle::new(42);
        let b = Handle::clone(&a);
        assert_eq!(*a, *b);
        assert_eq!(Handle::strong_count(&a), 2);
    }
}
