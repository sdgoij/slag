//! A tiny stack with inline capacity, for the matcher's task stack.
//!
//! The engine pushes a task per node it walks and pops it on completion, so a
//! one-atom pattern still grows a `Vec` from empty: one heap allocation per
//! `test` call, which is most of the engine's fixed cost on a short input (the
//! `regexp` perf harness counts exactly one alloc per `search_end_at` call on an
//! empty input). The first `N` entries live in the struct instead; only a
//! pattern deep enough to need more spills to the heap.

use std::mem::MaybeUninit;

/// `N` inline slots over a `Vec` spill.
pub(crate) struct SmallStack<T, const N: usize> {
    inline: [MaybeUninit<T>; N],
    /// The entry count. The live entries are `inline[..len]` while `spill` is
    /// empty, and `spill` (which then holds all `len`) once it is not.
    len: usize,
    spill: Vec<T>,
}

impl<T, const N: usize> SmallStack<T, N> {
    pub(crate) const fn new() -> Self {
        Self {
            inline: [const { MaybeUninit::uninit() }; N],
            len: 0,
            spill: Vec::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        if self.spill.is_empty() {
            // SAFETY: `inline[..len]` holds the initialized entries.
            unsafe { std::slice::from_raw_parts(self.inline.as_ptr() as *const T, self.len) }
        } else {
            &self.spill
        }
    }

    pub(crate) fn push(&mut self, value: T) {
        if self.spill.is_empty() {
            if self.len < N {
                self.inline[self.len].write(value);
                self.len += 1;
                return;
            }
            // The inline slots are full: move them into the spill, which is
            // where every later push goes.
            self.spill.reserve(N + 1);
            for slot in &mut self.inline {
                // SAFETY: all `N` inline entries are initialized here, and each
                // is moved out exactly once (the spill is the owner from now on).
                self.spill.push(unsafe { slot.assume_init_read() });
            }
        }
        self.spill.push(value);
        self.len += 1;
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        if self.spill.is_empty() {
            // SAFETY: entry `len` was the top of the inline stack; reading it
            // moves ownership to the caller.
            Some(unsafe { self.inline[self.len].assume_init_read() })
        } else {
            self.spill.pop()
        }
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        if self.spill.is_empty() {
            for index in len..self.len {
                // SAFETY: entries `[len, self.len)` are initialized and are being
                // discarded, so each is dropped exactly once.
                unsafe { self.inline[index].assume_init_drop() };
            }
        } else if len <= N {
            // Back to the inline form so `as_slice` and `pop` read the
            // survivors. `drain(..).take(len)` moves the survivors out and
            // drops the discarded tail when the iterator is dropped.
            for (index, value) in self.spill.drain(..).take(len).enumerate() {
                self.inline[index].write(value);
            }
        } else {
            self.spill.truncate(len);
        }
        self.len = len;
    }

    pub(crate) fn clear(&mut self) {
        self.truncate(0);
        self.spill.clear();
    }

    pub(crate) fn extend_from_slice(&mut self, values: &[T])
    where
        T: Clone,
    {
        for value in values {
            self.push(value.clone());
        }
    }
}

impl<T, const N: usize> Drop for SmallStack<T, N> {
    fn drop(&mut self) {
        // A non-empty spill owns every entry; only the inline form has live
        // values here.
        if self.spill.is_empty() {
            for index in 0..self.len {
                // SAFETY: entries below `len` are initialized when the inline
                // slots hold the stack.
                unsafe { self.inline[index].assume_init_drop() };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SmallStack;

    #[test]
    fn inline_until_the_cap_then_spill_then_back() {
        let mut stack: SmallStack<u32, 4> = SmallStack::new();
        assert!(stack.as_slice().is_empty());
        for value in 0..4 {
            stack.push(value);
        }
        assert_eq!(stack.as_slice(), &[0, 1, 2, 3]);
        assert!(stack.spill_is_empty_for_test());
        // The fifth push spills, keeping every entry in order.
        stack.push(4);
        assert_eq!(stack.as_slice(), &[0, 1, 2, 3, 4]);
        assert_eq!(stack.pop(), Some(4));
        assert_eq!(stack.pop(), Some(3));
        assert_eq!(stack.as_slice(), &[0, 1, 2]);
        // A truncate below the cap moves the survivors back inline.
        stack.push(9);
        stack.push(10);
        stack.truncate(2);
        assert_eq!(stack.as_slice(), &[0, 1]);
        assert!(stack.spill_is_empty_for_test());
        assert_eq!(stack.pop(), Some(1));
        stack.extend_from_slice(&[7, 8]);
        assert_eq!(stack.as_slice(), &[0, 7, 8]);
        stack.clear();
        assert!(stack.as_slice().is_empty());
    }

    #[test]
    fn a_dropped_spilled_stack_frees_every_entry_once() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let drops = Rc::new(RefCell::new(0));
        struct Tracked(Rc<RefCell<usize>>);
        impl Drop for Tracked {
            fn drop(&mut self) {
                *self.0.borrow_mut() += 1;
            }
        }
        {
            let mut stack: SmallStack<Tracked, 2> = SmallStack::new();
            for _ in 0..5 {
                stack.push(Tracked(Rc::clone(&drops)));
            }
            // Drop two while spilled, then truncate back under the cap so the
            // survivors move inline, then let the stack's own Drop run.
            let _ = stack.pop();
            let _ = stack.pop();
            stack.truncate(1);
        }
        assert_eq!(*drops.borrow(), 5, "every entry dropped exactly once");
    }

    impl<const N: usize> SmallStack<u32, N> {
        fn spill_is_empty_for_test(&self) -> bool {
            self.spill.is_empty()
        }
    }
}
