//! The V8-shaped [`Context`]: a realm on an isolate.

use crux::error::JsError;
use crux::handle::Handle;
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::realm::Realm;

use super::Isolate;
use super::handle::{Local, MaybeLocal};

/// A realm on an isolate (v8::Context).
///
/// The realm's bootstrap execution context is pushed when the context is
/// created, so with one context per isolate the context is always current.
/// The context holds a raw pointer to its isolate; the caller must not keep
/// a conflicting `&mut Isolate` alive while the context is in use (the same
/// borrow convention as the `crux::function::with_agent` TLS window).
///
/// Both fields are plain data, so the handle copies freely. It does not own
/// the realm: the isolate's agent does, for as long as the isolate lives.
#[derive(Clone, Copy)]
pub struct Context {
    isolate: *mut Isolate,
    realm: Handle<Realm>,
}

impl Context {
    /// InitializeHostDefinedRealm on `isolate` (spec 9.3.4) and push the
    /// bootstrap execution context.
    pub fn new(isolate: &mut Isolate) -> Result<Self, JsError> {
        Self::new_with_global_ops(isolate, None)
    }

    /// The same, over a host-defined global object: `global_ops` are the
    /// global's own internal methods (`crux::host::HostOps`), each falling back
    /// to the ordinary one, which is how a host installs a named property
    /// handler (V8's global template).
    pub fn new_with_global_ops(
        isolate: &mut Isolate,
        global_ops: Option<std::rc::Rc<dyn crux::host::HostOps>>,
    ) -> Result<Self, JsError> {
        let realm = isolate
            .agent
            .initialize_host_defined_realm_with_global(global_ops)?;
        Ok(Self {
            isolate: isolate as *mut Isolate,
            realm,
        })
    }

    /// The isolate this context runs on (valid while the isolate outlives
    /// the context).
    pub fn isolate(&self) -> *mut Isolate {
        self.isolate
    }

    /// The context for a realm the engine already made.
    ///
    /// The caller's contract: `isolate` is live and owns `realm`. Builtins that
    /// need to hand a host a context-shaped handle use this rather than
    /// `Context::new`, which would make a second realm.
    pub(crate) fn from_realm(isolate: *mut Isolate, realm: Handle<Realm>) -> Self {
        Self { isolate, realm }
    }

    /// The context of the realm whose global object `value` is
    /// (`v8::Object::GetCreationContext`).
    ///
    /// Narrowed, and the narrowing is the honest part: V8 answers the context an
    /// object was *created* in, for any object, and this engine records no such
    /// thing — a realm is a root and an object's containing realm is not written
    /// anywhere. What it does know is which realm's global object a value is, by
    /// the isolate's own realm list, and that is the case a host-defined global's
    /// interceptor callback is handed: deno's `vm` asks this of the holder of a
    /// property operation, which is the sandbox realm's global proxy. Anything
    /// else answers `None`, and a host that needs the general question needs a
    /// per-object realm record rather than a guess here.
    ///
    /// # Safety
    ///
    /// `isolate` must be live, which is what makes its agent's realm list
    /// readable; a caller has one from [`Context::isolate`] for as long as the
    /// scope it came from is open.
    pub unsafe fn of_global_object(isolate: *mut Isolate, value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let id = object.id();
        // SAFETY: the caller's contract.
        let isolate_ref = unsafe { &*isolate };
        let realm = isolate_ref
            .agent
            .realms
            .borrow()
            .iter()
            .copied()
            .find(|realm| realm.global_object.id() == id)?;
        Some(Self { isolate, realm })
    }

    /// The realm's global object.
    pub fn global(&self) -> Local {
        Local(Value::Object(self.realm.global_object))
    }

    /// Give this realm its own microtask queue
    /// (`v8::ContextOptions::microtask_queue`).
    ///
    /// The queue is the host's (`Isolate::new_microtask_queue`), and the id it
    /// answered is what the realm records: every promise job this realm enqueues
    /// goes there from now on, so the host drains that context's async work
    /// itself. Setting an id nothing knows (or one the host released) is not an
    /// error — the realm's jobs then take the default queue.
    pub fn set_microtask_queue(&self, id: u32) {
        self.realm.microtask_queue.set(Some(id));
    }

    /// An intrinsic value by `%`-name (e.g. `%Object.prototype%`).
    pub fn intrinsic(&self, name: &str) -> Option<Value> {
        self.realm.intrinsics.get(name)
    }

    /// Install this context's promise hooks (`v8::Context::SetPromiseHooks`).
    ///
    /// The four run where V8 runs them: `init` when a promise is created,
    /// handed the promise and its parent; `before` and `after` around a reaction
    /// job's handler; `resolve` when a promise settles. `None` leaves a slot
    /// unset, and an unset slot runs nothing — which is how a host installs only
    /// the kinds it wants. A hook sees this context's global object as its
    /// receiver, and one that throws fails the operation that ran it.
    pub fn set_promise_hooks(
        &self,
        init: Option<Local>,
        before: Option<Local>,
        after: Option<Local>,
        resolve: Option<Local>,
    ) {
        let mut hooks = self.realm.promise_hooks.borrow_mut();
        hooks.init = init.map(|hook| hook.0);
        hooks.before = before.map(|hook| hook.0);
        hooks.after = after.map(|hook| hook.0);
        hooks.resolve = resolve.map(|hook| hook.0);
    }

    /// The current realm.
    pub(crate) fn realm(&self) -> &Handle<Realm> {
        &self.realm
    }

    /// Write the data a host attached to each context, as one snapshot blob.
    ///
    /// The slots are the host's own numbering, in the crate we stand in for's
    /// convention: the default context is 0 and the contexts added after it are
    /// 1, 2, ... Each slot is written against the realm of the context it
    /// names, because a value's builtins are its own realm's. `externals` is the
    /// host's external-reference table — the addresses a snapshot may name by
    /// index instead of holding — and a host pointer that is not in it is an
    /// error naming that rather than a blob that cannot be loaded. A value a
    /// slot's realm cannot name is an error naming it too — see
    /// [`crate::snapshot::Unsupported`].
    ///
    /// An item is a value or a module record ([`crate::snapshot::SnapshotItem`]):
    /// a host attaches both, and a snapshot carries the *order* it attached them
    /// in, because the load hands them back by index.
    pub fn write_snapshot(
        slots: &[(usize, Context, Vec<crate::snapshot::SnapshotItem>)],
        externals: &[*mut std::ffi::c_void],
        host: Option<&dyn crate::snapshot::HostCallbacks>,
        realm_global: bool,
    ) -> Result<Vec<u8>, crate::snapshot::Unsupported> {
        let Some((_, first, _)) = slots.first() else {
            return Err(crate::snapshot::Unsupported::empty_table());
        };
        first.with_agent(|agent| {
            // The converted items are owned here rather than in the slot list
            // the encoder reads, which borrows them.
            let owned: Vec<(usize, Handle<Realm>, Vec<crate::snapshot::SnapshotItem>)> = slots
                .iter()
                .map(|(index, context, items)| (*index, *context.realm(), items.clone()))
                .collect();
            let engine_slots: Vec<crate::snapshot::Slot<'_>> = owned
                .iter()
                .map(|(index, realm, items)| crate::snapshot::Slot {
                    index: *index,
                    realm: *realm,
                    items,
                    realm_global,
                })
                .collect();
            let table: Vec<usize> = externals.iter().map(|pointer| *pointer as usize).collect();
            crate::snapshot::encode_slots(agent, &engine_slots, &table, host)
        })
    }

    /// Read the data a blob carried for one context slot, made in this realm.
    ///
    /// `None` when the blob names no such slot. The values are persistent
    /// handles: they are rooted from the moment they exist, so a host does not
    /// have to pin them itself, and dropping one releases it. `externals` is the
    /// host's table, rebuilt for this load; an index it does not have is an
    /// error rather than a read past the end.
    pub fn read_snapshot(
        &self,
        bytes: &[u8],
        slot: usize,
        externals: &[*mut std::ffi::c_void],
        host: Option<&dyn crate::snapshot::HostCallbacks>,
    ) -> Result<Option<Vec<crate::snapshot::SnapshotItem>>, crate::snapshot::DecodeError> {
        self.with_agent(|agent| {
            let table: Vec<usize> = externals.iter().map(|pointer| *pointer as usize).collect();
            crate::snapshot::decode_slot(agent, &self.realm, bytes, slot, &table, host)
        })
    }

    /// Run `body` with this isolate's agent recorded as current, so host
    /// callbacks can re-enter through [`Isolate::get_current`].
    pub fn with_agent<T>(&self, body: impl FnOnce(&mut Agent) -> T) -> T {
        let agent = unsafe { &*self.isolate }.agent_ptr();
        crux::function::with_agent(agent as *mut (), || body(unsafe { &mut *agent }))
    }

    /// Run `body` as one embedder entry.
    ///
    /// The depth is what makes `MicrotasksPolicy::Auto` mean "after the
    /// outermost entry": a host callback that calls back in raises it, so the
    /// queues cannot drain out from under the callback that is running. When
    /// the outermost entry returns and the policy is `Auto`, they drain — and a
    /// job that throws becomes the isolate's pending exception, where the crate
    /// we stand in for swallows it (the difference is a host's, and it is a
    /// rarer one: it can only be seen by asking).
    fn entered<T>(
        &self,
        body: impl FnOnce(&mut Agent) -> Result<T, JsError>,
    ) -> Result<T, JsError> {
        let isolate = unsafe { &*self.isolate };
        // A terminated isolate refuses the next entry rather than running it,
        // which is the one check point a host's own call needs: a script of two
        // statements has no loop and no call of its own.
        if isolate.is_execution_terminating() {
            return Err(crate::agent::termination_error());
        }
        let depth = isolate.entry_depth.get() + 1;
        isolate.entry_depth.set(depth);
        let result = self.with_agent(body);
        isolate.entry_depth.set(depth - 1);
        if depth == 1 {
            // The post-collection drain happens at the outermost exit, whatever
            // the microtask policy: a host that never drains jobs itself (the
            // engine's default is `Explicit`) would otherwise never see a
            // finalizer, and a finalizer is the collector's news to the host
            // rather than a job. It runs first — the host's own bookkeeping,
            // and a finalizer may enqueue a job the drain below then runs.
            self.with_agent(|agent| agent.run_host_finalizers());
            if isolate.get_microtasks_policy() == crate::api::MicrotasksPolicy::Auto
                && let Err(error) = self.with_agent(|agent| agent.run_jobs())
            {
                let thrown = self
                    .with_agent(|agent| crate::builtins::error::to_throwable(agent, &error))
                    .unwrap_or(Value::Undefined);
                isolate.set_pending_exception(thrown);
            }
        }
        result
    }

    /// Evaluate a Script; on failure the thrown value becomes the pending
    /// exception and the result is `Nothing` (v8::Script::Run semantics).
    pub fn eval(&self, source: &str) -> MaybeLocal {
        match self.try_eval(source) {
            Ok(value) => MaybeLocal::Some(value),
            Err(error) => {
                self.throw(&error);
                MaybeLocal::Nothing
            }
        }
    }

    /// Evaluate a Script, returning the engine error directly instead of
    /// setting a pending exception.
    pub fn try_eval(&self, source: &str) -> Result<Local, JsError> {
        self.try_eval_named(source, None)
    }

    /// Evaluate a Script the host named (v8::Script::Run): `name` is the
    /// script's resource name, which a host's dynamic-import callback is handed
    /// as the referrer of an `import()` written in this script.
    pub fn try_eval_named(&self, source: &str, name: Option<JsString>) -> Result<Local, JsError> {
        self.entered(|agent| {
            let value = agent.run_script_named(source, name)?;
            Ok(Local(value))
        })
    }

    /// Call a function; on failure the thrown value becomes the pending
    /// exception (v8::Function::Call semantics).
    pub fn call(&self, function: &Local, this: &Local, args: &[Local]) -> MaybeLocal {
        match self.try_call(function, this, args) {
            Ok(value) => MaybeLocal::Some(value),
            Err(error) => {
                self.throw(&error);
                MaybeLocal::Nothing
            }
        }
    }

    /// Call a function, returning the engine error directly.
    pub fn try_call(
        &self,
        function: &Local,
        this: &Local,
        args: &[Local],
    ) -> Result<Local, JsError> {
        let values: Vec<Value> = args.iter().map(|arg| arg.into_value()).collect();
        self.entered(|agent| {
            let result =
                crate::function::call(agent, function.value(), this.into_value(), &values)?;
            Ok(Local(result))
        })
    }

    /// Construct an object from a constructor; on failure the thrown value
    /// becomes the pending exception.
    pub fn construct(&self, constructor: &Local, args: &[Local]) -> MaybeLocal {
        match self.try_construct(constructor, args) {
            Ok(value) => MaybeLocal::Some(value),
            Err(error) => {
                self.throw(&error);
                MaybeLocal::Nothing
            }
        }
    }

    /// Construct an object, returning the engine error directly.
    pub fn try_construct(&self, constructor: &Local, args: &[Local]) -> Result<Local, JsError> {
        let values: Vec<Value> = args.iter().map(|arg| arg.into_value()).collect();
        self.entered(|agent| {
            let result = crate::function::construct(
                agent,
                constructor.value(),
                &values,
                constructor.value(),
            )?;
            Ok(Local(result))
        })
    }

    /// Add a microtask: `callback` runs with no arguments when the job queues
    /// drain (v8::Isolate::EnqueueMicrotask).
    ///
    /// The engine's microtasks are its promise jobs, so the callback runs as
    /// one, in this context's realm and behind the jobs already queued.
    pub fn enqueue_microtask(&self, callback: Local) {
        let realm = self.realm;
        self.with_agent(|agent| {
            agent.enqueue_promise_job(Some(realm), move |agent| {
                crate::function::call(agent, callback.value(), Value::Undefined, &[])
            });
        });
    }

    /// Drain the job queues (microtasks, timers, generic jobs).
    pub fn run_microtasks(&self) -> Result<(), JsError> {
        self.with_agent(|agent| agent.run_jobs())
    }

    /// A marker RAII: with one context per isolate the context is always
    /// current, so the scope is advisory (v8::Context::Scope).
    pub fn scope(&self) -> ContextScope<'_> {
        ContextScope(std::marker::PhantomData)
    }

    /// Convert an engine error into a thrown value (spec ch. 17: a real
    /// Error object when the built-ins are installed) and set it as the
    /// pending exception.
    fn throw(&self, error: &JsError) {
        let value = self
            .with_agent(|agent| crate::builtins::error::to_throwable(agent, error))
            .unwrap_or_else(|_| Value::String(Handle::new(JsString::from_utf8(&error.message))));
        unsafe { &*self.isolate }.set_pending_exception(value);
    }
}

/// RAII marker for a current context (v8::Context::Scope). Advisory with one
/// context per isolate.
pub struct ContextScope<'a>(std::marker::PhantomData<&'a Context>);

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crux::function::Function;
    use crux::string::JsString;

    use super::*;
    use crate::api::Isolate;

    /// The four promise hooks a context installs run where V8 runs them, with
    /// V8's own call shape: an init is handed the promise and its parent, every
    /// other kind the promise alone, and the receiver is the global object.
    #[test]
    fn a_contexts_promise_hooks_run_with_v8s_call_shape() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");
        let global = context.global().into_value();

        let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let hook = |kind: &'static str, length: u64| -> Local {
            let seen = seen.clone();
            let function = Function::create_builtin(
                Some(JsString::from_utf8("")),
                length,
                Box::new(move |this: &Value, args: &[Value]| {
                    seen.borrow_mut().push(format!(
                        "{kind}:{}:receiver={}",
                        args.len(),
                        *this == global
                    ));
                    Ok(Value::Undefined)
                }),
                None,
                None,
            )
            .expect("hook");
            Local(Value::Function(function))
        };
        context.set_promise_hooks(
            Some(hook("init", 2)),
            Some(hook("before", 1)),
            Some(hook("after", 1)),
            Some(hook("resolve", 1)),
        );

        context
            .try_eval("Promise.resolve(1).then(function () {})")
            .expect("eval");
        context.run_microtasks().expect("microtasks");

        let seen = seen.borrow().clone();
        for expected in [
            "init:2:receiver=true",
            "resolve:1:receiver=true",
            "before:1:receiver=true",
            "after:1:receiver=true",
        ] {
            assert!(
                seen.iter().any(|call| call == expected),
                "{expected} was not among {seen:?}"
            );
        }
    }

    /// A slot a context left unset runs nothing, which is how a host installs
    /// only the kinds it wants.
    #[test]
    fn an_unset_promise_hook_slot_runs_nothing() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");

        let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let seen_hook = seen.clone();
        let resolve = Function::create_builtin(
            Some(JsString::from_utf8("")),
            1,
            Box::new(move |_: &Value, _: &[Value]| {
                seen_hook.borrow_mut().push("resolve".to_string());
                Ok(Value::Undefined)
            }),
            None,
            None,
        )
        .expect("hook");
        context.set_promise_hooks(None, None, None, Some(Local(Value::Function(resolve))));

        context
            .try_eval("Promise.resolve(1).then(function () {})")
            .expect("eval");
        context.run_microtasks().expect("microtasks");

        let seen = seen.borrow().clone();
        assert!(!seen.is_empty(), "the resolve hook ran");
        assert!(
            seen.iter().all(|call| call == "resolve"),
            "only the installed slot ran: {seen:?}"
        );
    }
}
