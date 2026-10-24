//! An isolate that belongs to no thread.
//!
//! [`SharedIsolate`] is the crate we stand in for's `v8::SharedIsolate`: the
//! isolate is owned by a lock rather than by a thread, and every use enters it
//! by taking a [`Locker`]. The lock is what serialises turns, and the locker
//! installs the isolate's thread-local state — its heap and its handle arena —
//! for the life of the turn, which is the same pair a `HandleScope` installs.

use std::ops::{Deref, DerefMut};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::isolate::{Isolate, OwnedIsolate};

/// An isolate that may be entered from any thread (`v8::SharedIsolate`).
pub struct SharedIsolate {
    inner: Mutex<Shared>,
    /// The watchdog handle, made before the isolate went behind the lock: its
    /// whole point is that a thread that does not hold the lock can use it, so
    /// it cannot be built through `lock`.
    handle: crate::IsolateHandle,
}

struct Shared {
    isolate: OwnedIsolate,
}

// SAFETY: the mutex is the only way to reach `isolate`, and holding it is what
// makes the holder the isolate's thread: `Locker::new` installs the isolate's
// heap and handle arena, and its `Drop` removes them. A thread that does not
// hold the lock therefore never reads or writes any of the `!Send` state — the
// arenas, the tables, the `Rc`s — which is the invariant the `!Send` fields
// need. `OwnedIsolate::try_into_shared` is `unsafe` because the caller has to
// promise that nothing else reaches the isolate.
unsafe impl Send for Shared {}

/// Why [`OwnedIsolate::try_into_shared`] refused to share an isolate.
///
/// The refusals are the crate we stand in for's: a state that remembers a
/// thread, or an isolate still in use, cannot be given to a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedIsolateError {
    /// The isolate carries a snapshot creator, which names its own isolate.
    SnapshotCreator,
    /// A handle scope is open over the isolate: some scope still owns its
    /// region, and would close it on another thread's stack.
    OpenScope,
    /// The isolate holds weak handles. A `WeakHandle` is `!Send` by
    /// construction, so one in the heap means a host still holds a handle
    /// bound to the thread that made it.
    WeakHandles,
}

impl std::fmt::Display for SharedIsolateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SnapshotCreator => "the isolate carries a snapshot creator",
            Self::OpenScope => "a handle scope is open over the isolate",
            Self::WeakHandles => "the isolate holds weak handles, which are thread-bound",
        })
    }
}

impl std::error::Error for SharedIsolateError {}

impl SharedIsolate {
    pub(crate) fn new(isolate: OwnedIsolate) -> Self {
        let handle = isolate.thread_safe_handle();
        Self {
            inner: Mutex::new(Shared { isolate }),
            handle,
        }
    }

    /// The handle a watchdog keeps (`v8::Isolate::GetThreadSafeHandle`):
    /// `Send + Sync`, and usable without the lock.
    pub fn thread_safe_handle(&self) -> crate::IsolateHandle {
        self.handle.clone()
    }

    /// Take the isolate for one turn, blocking until the current holder
    /// releases it (`v8::Locker::lock` / `SharedIsolate::lock`).
    ///
    /// The returned [`Locker`] derefs to the isolate and uninstalls the
    /// thread-local entry when it drops. A panic while the lock is held
    /// poisons it; the isolate's state is still consistent (a panic unwinds
    /// through the `Locker`'s `Drop`), so the poison is recovered rather than
    /// propagated — refusing every later turn would make one bad turn
    /// permanent.
    pub fn lock(&self) -> Locker<'_> {
        let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        Locker::new(guard)
    }
}

/// One turn with a [`SharedIsolate`].
pub struct Locker<'a> {
    guard: MutexGuard<'a, Shared>,
}

impl<'a> Locker<'a> {
    fn new(guard: MutexGuard<'a, Shared>) -> Self {
        let isolate: &Isolate = &guard.isolate;
        crux::heap::enter(isolate.heap_ptr());
        crate::store::enter(isolate.table_ptr());
        Self { guard }
    }
}

impl Drop for Locker<'_> {
    fn drop(&mut self) {
        crate::store::leave();
        crux::heap::leave();
    }
}

impl Deref for Locker<'_> {
    type Target = Isolate;

    fn deref(&self) -> &Self::Target {
        &self.guard.isolate
    }
}

impl DerefMut for Locker<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard.isolate
    }
}

impl AsMut<Isolate> for Locker<'_> {
    fn as_mut(&mut self) -> &mut Isolate {
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::eval;
    use crate::{Context, ContextScope, CreateParams, Isolate};

    /// A shared isolate is entered from more than one thread, one turn at a
    /// time: the locker installs the isolate's heap and handle arena for the
    /// turn, which is what makes its `!Send` state reachable at all. Without
    /// that entry the second thread's realm would allocate in a heap the first
    /// one owns.
    #[test]
    fn a_shared_isolate_is_entered_from_any_thread() {
        let isolate = Isolate::new(CreateParams::default());
        // SAFETY: the isolate was just made and nothing else holds it.
        let isolate = unsafe { isolate.try_into_shared() }.expect("a fresh isolate can be shared");

        let answers: Vec<String> = std::thread::scope(|threads| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    threads.spawn(|| {
                        let mut locker = isolate.lock();
                        crate::scope!(let scope, &mut *locker);
                        let context = Context::new(scope, Default::default());
                        let scope = &mut ContextScope::new(scope, context);
                        eval(scope, "1 + 1").to_rust_string_lossy(scope)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("a turn"))
                .collect()
        });

        assert_eq!(answers, ["2", "2"]);
    }
}
