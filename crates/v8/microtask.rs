//! A microtask queue the host owns and drains itself (`v8::MicrotaskQueue`).
//!
//! The engine has one queue per isolate and — since the ladder's L3 work — queues
//! a *host* makes and drains on its own schedule. This is the host's handle to
//! one: it names the engine queue a context attaches
//! (`ContextOptions::microtask_queue`) so that context's promise jobs stop
//! joining the isolate's own turn, and [`MicrotaskQueue::perform_checkpoint`] is
//! how the host runs exactly them. `deno/ext/node/ops/vm.rs` is what asks for it:
//! a `vm.Context` with `microtaskMode: 'afterEvaluate'` gets its own queue, and
//! `vm` drains it when the evaluated script returns.
//!
//! # The token, and what it owes the isolate
//!
//! The crate we stand in for hands out a `UniqueRef` that the host turns into a
//! raw pointer with `into_raw`, keeps for as long as the queue is needed, and
//! drops with `drop_in_place`. Here that token is this struct, and it carries the
//! isolate because its `Drop` *is* the release: the engine's queue goes quiet and
//! the realm's later jobs take the default queue. The crate's own ownership
//! contract applies unchanged — the token must be dropped before its isolate, and
//! what was attached to the queue is the host's to have let go of first.
//!
//! The one legitimate way both go at once is the isolate's own teardown, and the
//! bridge makes that safe rather than leaving it to the host's luck:
//! `IsolateInner`'s `Drop` terminates the host object heap before the engine
//! field drops, so a `GarbageCollected` value's destructor — which is where deno
//! drops this token from, in `ContextifyContext::drop` — still finds the engine
//! alive. That is also V8's own order (`Isolate::Dispose` terminates cppgc
//! before the engine's state goes), so the guarantee is the host's either way.

use runtime::api::MicrotasksPolicy;

use crate::isolate::Isolate;
use crate::support::UniqueRef;

/// A queue of microtasks the host drains itself (`v8::MicrotaskQueue`).
pub struct MicrotaskQueue {
    /// The isolate whose engine queue this names, kept for the release `Drop`
    /// performs. A copy of the handle, not a borrow: a token outlives the scope
    /// it was made in, which is the whole point of `into_raw`.
    isolate: Isolate,
    /// The engine's queue, as the isolate's `new_microtask_queue` answered it and
    /// the context's `set_microtask_queue` takes it. Opaque to the host either way.
    pub(crate) id: u32,
}

impl MicrotaskQueue {
    /// Make a queue on `isolate` (`v8::MicrotaskQueue::new`).
    ///
    /// `policy` is the queue's own: `Explicit` — what deno's `vm` asks for — is
    /// drained only by [`perform_checkpoint`](Self::perform_checkpoint), while
    /// `Auto` is also drained by the isolate's own run.
    pub fn new(isolate: &mut Isolate, policy: MicrotasksPolicy) -> UniqueRef<Self> {
        let id = isolate.engine_mut().new_microtask_queue(policy);
        UniqueRef::new(Self {
            isolate: *isolate,
            id,
        })
    }

    /// Run this queue's jobs to completion
    /// (`v8::MicrotaskQueue::perform_checkpoint`).
    ///
    /// Only this queue: the isolate's own queue and any other host queue are
    /// untouched, and work the jobs enqueue for this queue's realm comes back
    /// here. `isolate` is the one the queue was made on, as the crate we stand in
    /// for requires; this token carries that isolate too, so the two must agree.
    ///
    /// One divergence, the same one the isolate's checkpoint records: the crate
    /// we stand in for swallows what a job throws, and this makes it the pending
    /// exception instead, so a host that wants to see it can.
    pub fn perform_checkpoint(&self, isolate: &mut Isolate) {
        // A handle scope for the drain, as the isolate's own checkpoint opens: a
        // job may run a host callback, which builds handles and needs a region.
        crate::scope!(let scope, isolate);
        if let Err(error) = scope.engine_mut().run_microtask_queue(self.id) {
            crate::throw(scope, &error);
        }
    }
}

impl Drop for MicrotaskQueue {
    /// Release the engine queue, which is what dropping the token does there. An
    /// id is never reused, so a stale one is a no-op, and a realm still attached
    /// to it falls back to the default queue.
    fn drop(&mut self) {
        let mut isolate = self.isolate;
        isolate.engine_mut().release_microtask_queue(self.id);
    }
}

#[cfg(test)]
mod tests {
    use crate::isolate::CreateParams;
    use crate::scope::ContextScope;
    use crate::{Context, ContextOptions, Isolate, MicrotasksPolicy, PinScope};

    use super::MicrotaskQueue;

    /// A context with its own `Explicit` queue, and the raw token the way a host
    /// holds it: `into_raw` out of the `UniqueRef`, attached through
    /// `ContextOptions`, released by `drop_in_place` or by the isolate's teardown.
    fn with_queued_context(body: impl FnOnce(&mut PinScope<'_, '_, Context>, *mut MicrotaskQueue)) {
        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let queue = MicrotaskQueue::new(handle_scope, MicrotasksPolicy::Explicit).into_raw();
        let context = Context::new(
            handle_scope,
            ContextOptions {
                microtask_queue: Some(queue),
                ..Default::default()
            },
        );
        let scope = &mut ContextScope::new(handle_scope, context);
        body(scope, queue);
    }

    /// Set `globalThis.ran` to 0 and queue a job that sets it to `value`, which is
    /// the shape deno's `vm` sees: the script returns and the continuation is
    /// still pending.
    fn queue_a_job(scope: &mut PinScope<'_, '_, Context>, value: i32) {
        crate::test_support::eval(
            scope,
            &format!(
                "globalThis.ran = 0;\
                 Promise.resolve().then(() => {{ globalThis.ran = {value}; }});"
            ),
        );
    }

    fn ran(scope: &PinScope<'_, '_, Context>) -> f64 {
        crate::test_support::eval_number(scope, "globalThis.ran")
    }

    /// The feature's whole point: a context's own queue keeps its jobs out of the
    /// isolate's run, and the host's checkpoint runs exactly them. `eval` leaves
    /// the job pending (the isolate's policy here is `Explicit`), `run_microtasks`
    /// must not reach it, and `perform_checkpoint` must.
    #[test]
    fn a_host_queue_is_the_hosts_to_drain() {
        with_queued_context(|scope, queue| {
            queue_a_job(scope, 1);

            scope.run_microtasks().expect("microtasks");
            assert_eq!(
                ran(scope),
                0.0,
                "the isolate's own run leaves the host's queue alone"
            );

            // SAFETY: as a host holds it — the pointer `into_raw` handed out, not
            // yet dropped.
            unsafe { &*queue }.perform_checkpoint(scope);
            assert_eq!(
                ran(scope),
                1.0,
                "the host's checkpoint is what drains that queue"
            );
        });
    }

    /// A null `ContextOptions::microtask_queue`, which is what deno passes when
    /// `own_microtask_queue` is false, means "no queue": the realm keeps the
    /// isolate's. Reading it as a queue would be a null dereference, and reading
    /// it as id 0 would silently attach the realm to somebody else's queue.
    #[test]
    fn a_null_queue_leaves_the_realm_on_the_isolates() {
        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        // A real queue, so a null that is misread as id 0 would find it rather
        // than a hole: the realm's job must still take the isolate's own queue.
        let first = MicrotaskQueue::new(handle_scope, MicrotasksPolicy::Explicit).into_raw();
        let context = Context::new(
            handle_scope,
            ContextOptions {
                microtask_queue: Some(std::ptr::null_mut()),
                ..Default::default()
            },
        );
        let scope = &mut ContextScope::new(handle_scope, context);

        queue_a_job(scope, 5);
        scope.run_microtasks().expect("microtasks");
        assert_eq!(ran(scope), 5.0, "a null queue is no queue");

        // SAFETY: as a host holds it; the queue is real, just unattached.
        unsafe { &*first }.perform_checkpoint(scope);
        assert_eq!(ran(scope), 5.0, "and the realm never went to the other one");
    }

    /// Dropping the token releases the engine queue — the crate's `drop_in_place`
    /// path — and a realm still attached to it then falls back to the default
    /// queue. Without the release the second job would sit in a queue nobody
    /// drains, which is what makes this the test of the `Drop`.
    #[test]
    fn dropping_the_token_releases_the_queue() {
        with_queued_context(|scope, queue| {
            queue_a_job(scope, 1);
            // SAFETY: as a host holds it.
            unsafe { &*queue }.perform_checkpoint(scope);
            assert_eq!(ran(scope), 1.0);

            // SAFETY: the host's own release, which is `drop_in_place` and not
            // `Box::from_raw` — `into_raw` boxes the token and leaks the box, as
            // `support.rs` records, so the box is not freed here.
            unsafe { std::ptr::drop_in_place(queue) };

            queue_a_job(scope, 2);
            scope.run_microtasks().expect("microtasks");
            assert_eq!(
                ran(scope),
                2.0,
                "after the release the realm's jobs take the default queue"
            );
        });
    }

    /// An `Auto` queue is drained by the isolate's own run as well — the policy's
    /// whole meaning — so a job in one does not wait for the host.
    #[test]
    fn an_auto_queue_is_drained_by_the_isolates_own_run() {
        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let queue = MicrotaskQueue::new(handle_scope, MicrotasksPolicy::Auto).into_raw();
        let context = Context::new(
            handle_scope,
            ContextOptions {
                microtask_queue: Some(queue),
                ..Default::default()
            },
        );
        let scope = &mut ContextScope::new(handle_scope, context);

        queue_a_job(scope, 7);
        scope.run_microtasks().expect("microtasks");
        assert_eq!(
            ran(scope),
            7.0,
            "an Auto queue's jobs drain with the isolate's own run"
        );
    }
}
