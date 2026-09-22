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
        let realm = isolate.agent.initialize_host_defined_realm()?;
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

    /// The realm's global object.
    pub fn global(&self) -> Local {
        Local(Value::Object(self.realm.global_object))
    }

    /// An intrinsic value by `%`-name (e.g. `%Object.prototype%`).
    pub fn intrinsic(&self, name: &str) -> Option<Value> {
        self.realm.intrinsics.get(name)
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
        let depth = isolate.entry_depth.get() + 1;
        isolate.entry_depth.set(depth);
        let result = self.with_agent(body);
        isolate.entry_depth.set(depth - 1);
        if depth == 1
            && isolate.get_microtasks_policy() == crate::api::MicrotasksPolicy::Auto
            && let Err(error) = self.with_agent(|agent| agent.run_jobs())
        {
            let thrown = self
                .with_agent(|agent| crate::builtins::error::to_throwable(agent, &error))
                .unwrap_or(Value::Undefined);
            isolate.set_pending_exception(thrown);
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
        self.entered(|agent| {
            let value = agent.run_script(source)?;
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
