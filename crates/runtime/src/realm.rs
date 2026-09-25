//! Realms (spec 9.3): Realm Records, the intrinsic registry, and the
//! bootstrap pipeline (CreateIntrinsics, NewGlobalEnvironment,
//! SetDefaultGlobalBindings).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::heap::{GcAny, Trace};
use crux::host::HostOps;
use crux::object::{JsObject, PropertyKind};
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::env::{EnvRef, new_global_environment};

/// A Realm Record (spec 9.3 table): the intrinsic registry, the global
/// object, and the global environment.
#[derive(Debug)]
pub struct Realm {
    pub agent_signifier: u64,
    pub intrinsics: Intrinsics,
    pub global_object: Handle<JsObject>,
    pub global_env: EnvRef,
    /// The four promise hooks this context installed
    /// (`v8::Context::SetPromiseHooks`): run when a promise is created, before
    /// and after a reaction job's handler, and when a promise settles. V8 keeps
    /// the same four slots on the native context and reads them from the
    /// *current* context at each of those points, which is what this is.
    pub promise_hooks: RefCell<PromiseHooks>,
    /// [[LoadedModules]] (spec 9.3): the Source Text Module Records keyed by
    /// resolved specifier.
    pub loaded_modules:
        RefCell<std::collections::HashMap<JsString, Handle<crate::module::SourceTextModule>>>,
    /// The microtask queue this realm's promise jobs go to, when the host gave
    /// the context its own (`v8::ContextOptions::microtask_queue`). A `Cell`
    /// because a realm exists before the host's option is applied to it, and
    /// `None` is the agent's own queue.
    pub microtask_queue: std::cell::Cell<Option<u32>>,
}

/// The promise hooks a context can install, one slot per
/// `v8::PromiseHookType`.
///
/// `None` is V8's `undefined` slot: nothing is run for that kind, which is how
/// a host installs only the ones it wants (deno's two timer tests install one
/// each).
#[derive(Default, Debug)]
pub struct PromiseHooks {
    /// Runs when a promise is created, with the promise and its parent.
    pub init: Option<Value>,
    /// Runs before a reaction job's handler.
    pub before: Option<Value>,
    /// Runs after that handler returned.
    pub after: Option<Value>,
    /// Runs when a promise settles.
    pub resolve: Option<Value>,
}

impl Trace for PromiseHooks {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.init.trace(visit);
        self.before.trace(visit);
        self.after.trace(visit);
        self.resolve.trace(visit);
    }
}

impl Trace for Realm {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.intrinsics.trace(visit);
        self.global_object.trace(visit);
        self.global_env.trace(visit);
        // Both are RefCells: `RefCell<T>`'s trace skips a cell that is
        // mutably borrowed mid-collection (per-allocation `--gc-stress`) and
        // aborts the sweep instead of panicking.
        self.promise_hooks.trace(visit);
        self.loaded_modules.trace(visit);
    }
}

impl Realm {
    pub fn global_env(&self) -> EnvRef {
        self.global_env
    }
}

/// The intrinsic names a chain dispatcher's callee was defined under: what
/// `Intrinsics::name_of` resolves once per call, so the dispatcher's arms can
/// answer "is the callee the intrinsic named `X`?" with a string compare
/// instead of an `Intrinsics::get` probe per arm.
pub struct ResolvedNames(Option<Rc<Vec<Rc<str>>>>);

impl ResolvedNames {
    /// Is the callee the intrinsic defined under `name`? (A function object
    /// can hold more than one name, so this is a membership test.)
    #[inline]
    pub fn is(&self, name: &str) -> bool {
        self.0
            .as_deref()
            .is_some_and(|names| names.iter().any(|n| &**n == name))
    }

    /// Every name the callee was defined under, empty when it is not one of
    /// this realm's intrinsics. A dispatcher that fronts several others (the
    /// Intl and Temporal namespace routers) reads the namespace off a name's
    /// prefix before it compares whole names.
    #[inline]
    pub fn names(&self) -> &[Rc<str>] {
        match &self.0 {
            Some(names) => names.as_slice(),
            None => &[],
        }
    }
}

/// The intrinsic registry (spec 9.3.1): %-named values installed by each
/// built-in phase, in spec bootstrap order.
#[derive(Debug, Default)]
pub struct Intrinsics {
    /// Every intrinsic by name, keyed by `Rc<str>` rather than `JsString` so a
    /// lookup by name borrows the key instead of encoding a UTF-16 string per
    /// probe (`JsString` is UTF-16, so `from_utf8` allocates on every probe).
    /// The identity-chain dispatchers are the hot path:
    /// `temporal::shell::dispatch_call` alone probes up to 111 names per call.
    entries: RefCell<HashMap<Rc<str>, Value>>,
    /// Every name each entry was defined under, by function id: a function
    /// object can hold more than one name (spec aliases such as
    /// `%Array.prototype.values%` / `%Array.prototype[Symbol.iterator]%` are
    /// the same object), so this is a set per id. `define` shares the `Rc`
    /// with `entries`, so the index costs a refcount bump per intrinsic, and
    /// the identity-chain dispatchers resolve their callee's names once per
    /// call and compare strings instead of probing the table per arm.
    names: RefCell<HashMap<u64, Rc<Vec<Rc<str>>>>>,
    /// The names `name_members` derived rather than an install declared. The
    /// pass scans owners to derive their members, so a derived name must not be
    /// scanned as an owner in turn: `%Array.prototype.constructor%` names
    /// `%Array%`, and treating it as an owner would mint
    /// `%Array.prototype.constructor.from%` for a member that already has the
    /// declared spelling `%Array.from%`. Skipping them is what makes the pass
    /// idempotent, which the pass's own contract promises.
    derived: RefCell<HashSet<Rc<str>>>,
    /// Cut 26: the realm's %Object.prototype% handle, cached after the first
    /// resolution — the intrinsics table is populated at bootstrap and never
    /// reassigned, so the handle is stable for the realm's life. Object
    /// literals (`ObjectBegin`) and constructor `this` fallbacks read it per
    /// object creation.
    object_prototype: RefCell<Option<Value>>,
    /// The realm's %Array.prototype% handle, cached after the first
    /// resolution like `object_prototype`: every array creation
    /// (`array_create` — array literals, `new Array`, the array builtins)
    /// reads it, and `Intrinsics::get` allocates a JsString per call.
    array_prototype: RefCell<Option<Value>>,
    /// Cut 66: the function-creation prototype intrinsics (%Function.prototype%
    /// and the generator/async variants), resolved once per realm like
    /// `object_prototype` — `set_function_prototype`/`make_constructor` read
    /// them on every closure creation, and `Intrinsics::get` allocates a
    /// JsString per call. A fixed array indexed by [`function_prototype_index`]
    /// keeps the read a plain borrow + load (no hashing at all).
    function_prototypes: RefCell<[Option<Value>; FN_PROTO_COUNT]>,
    /// The realm's %Function.prototype.apply% / %Function.prototype.call%
    /// builtins, cached after the first resolution: the compiled `CallApply`
    /// step compares the member-read result against these per call, and the
    /// values are stable for the realm's life (the intrinsics table is
    /// populated at bootstrap and never reassigned).
    apply_builtin: RefCell<Option<Value>>,
    call_builtin: RefCell<Option<Value>>,
    /// The realm's %String.prototype% value, cached after the first
    /// resolution: primitive-string member reads resolve their chain
    /// directly against it instead of boxing a per-read String-exotic
    /// wrapper (tasklist 1.4 follow-on). `Intrinsics::get` allocates a
    /// JsString per call, so the cache keeps a hot `s.charAt`-style read off
    /// the allocation path.
    string_prototype: RefCell<Option<Value>>,
    /// The other primitive-creation prototype intrinsics (%Number.prototype%,
    /// %Boolean.prototype%, %BigInt.prototype%, %Symbol.prototype%), cached
    /// after the first resolution like `string_prototype`: non-string
    /// primitive member reads resolve their chain against these directly
    /// (their wrappers are ordinary objects with no own properties, so a
    /// chain read with the primitive receiver is exact).
    primitive_prototypes: RefCell<[Option<Value>; PRIM_PROTO_COUNT]>,
    /// The realm this table belongs to — a back-reference for the A2 write
    /// barrier: a builtin installed lazily (first use) into an old realm is an
    /// old->young edge. Deliberately not traced: the table lives inside its
    /// realm, so this edge can never be the only path to anything.
    owner: Cell<Option<Handle<Realm>>>,
}

/// The number of cached function-creation prototype intrinsics.
pub(crate) const FN_PROTO_COUNT: usize = 6;

/// The number of cached non-string primitive-creation prototype intrinsics
/// (the primitive member-read fast path resolves chain reads against them).
pub(crate) const PRIM_PROTO_COUNT: usize = 4;

/// The slot index for a non-string primitive prototype intrinsic name.
pub(crate) fn primitive_prototype_index(name: &str) -> usize {
    match name {
        "%Number.prototype%" => 0,
        "%Boolean.prototype%" => 1,
        "%BigInt.prototype%" => 2,
        "%Symbol.prototype%" => 3,
        _ => unreachable!("not a cached primitive prototype intrinsic: {name}"),
    }
}

/// The slot index for a function-creation prototype intrinsic name.
pub(crate) fn function_prototype_index(name: &str) -> usize {
    match name {
        "%Function.prototype%" => 0,
        "%GeneratorFunction.prototype%" => 1,
        "%AsyncFunction.prototype%" => 2,
        "%AsyncGeneratorFunction.prototype%" => 3,
        "%Generator.prototype%" => 4,
        "%AsyncGenerator.prototype%" => 5,
        _ => unreachable!("not a cached function-creation intrinsic: {name}"),
    }
}

impl Trace for Intrinsics {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        // The cells are RefCells: `RefCell<T>`'s trace skips a cell that is
        // mutably borrowed mid-collection (per-allocation `--gc-stress`) and
        // aborts the sweep instead of panicking.
        self.entries.trace(visit);
        self.object_prototype.trace(visit);
        self.array_prototype.trace(visit);
        match self.function_prototypes.try_borrow() {
            Ok(guard) => {
                for slot in guard.iter() {
                    slot.trace(visit);
                }
            }
            Err(_) => crux::heap::note_aborted_trace(),
        }
        self.apply_builtin.trace(visit);
        self.call_builtin.trace(visit);
        self.string_prototype.trace(visit);
        match self.primitive_prototypes.try_borrow() {
            Ok(guard) => {
                for slot in guard.iter() {
                    slot.trace(visit);
                }
            }
            Err(_) => crux::heap::note_aborted_trace(),
        }
    }
}

impl Intrinsics {
    /// Record the realm this table is embedded in, so the write barrier can
    /// name the box an intrinsic installation belongs to (A2).
    pub fn set_owner(&self, realm: Handle<Realm>) {
        self.owner.set(Some(realm));
    }

    /// The A2 write barrier for a store into one of the cached-value fields:
    /// the table lives inside its realm box, so the owner back-reference is
    /// the only way to name the box being written.
    fn cache_barrier(&self, value: Value) {
        if let Some(realm) = self.owner.get() {
            crux::heap::write_barrier(&*realm, value);
        }
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        self.entries.borrow().get(name).cloned()
    }

    /// The intrinsic names `callee` was defined under, if it is one of this
    /// realm's intrinsics. One lookup, against the per-arm table probes the
    /// chain dispatchers used to do (`temporal::shell::dispatch_call` walks up
    /// to 111 arms per call, and each probe encoded a `JsString`).
    pub fn name_of(&self, callee: &Value) -> ResolvedNames {
        let id = callee.as_function().map(|function| function.id());
        ResolvedNames(id.and_then(|id| self.names.borrow().get(&id).cloned()))
    }

    /// The name an intrinsic value is registered under, when it is one of this
    /// realm's, compared by identity: a string that spells an intrinsic's name
    /// is not that intrinsic, and two symbols with one description are two
    /// symbols. Named rather than unnamed because a snapshot writes a reference
    /// to a builtin as this name and a restore resolves it in the realm it
    /// rebuilds, which is what keeps a prototype or a constructor out of the
    /// graph the snapshot has to carry.
    ///
    /// Functions are answered by the id-keyed name table `name_of` reads;
    /// everything else — %Object.prototype% and its siblings — has no name
    /// record, so the entry table is scanned by identity.
    pub fn name_of_value(&self, value: &Value) -> Option<Rc<str>> {
        if let Some(name) = self.name_of(value).names().first() {
            return Some(Rc::clone(name));
        }
        let id = value.as_object().map(|object| object.id())?;
        self.entries
            .borrow()
            .iter()
            .find(|(_, entry)| entry.as_object().is_some_and(|entry| entry.id() == id))
            .map(|(name, _)| Rc::clone(name))
    }

    /// The realm's %Object.prototype% value, cached after the first
    /// resolution (see the struct field).
    pub fn object_prototype(&self) -> Option<Value> {
        if let Some(value) = self.object_prototype.borrow().as_ref() {
            return Some(*value);
        }
        let value = self.get("%Object.prototype%")?;
        self.cache_barrier(value);
        *self.object_prototype.borrow_mut() = Some(value);
        Some(value)
    }

    /// The realm's %Array.prototype% value, cached after the first
    /// resolution (see the struct field).
    pub fn array_prototype(&self) -> Option<Value> {
        if let Some(value) = self.array_prototype.borrow().as_ref() {
            return Some(*value);
        }
        let value = self.get("%Array.prototype%")?;
        self.cache_barrier(value);
        *self.array_prototype.borrow_mut() = Some(value);
        Some(value)
    }

    /// A function-creation prototype intrinsic (%Function.prototype%,
    /// %GeneratorFunction.prototype%, %AsyncFunction.prototype%,
    /// %AsyncGeneratorFunction.prototype%, %Generator.prototype%,
    /// %AsyncGenerator.prototype%), cached after the first resolution (see
    /// the struct field).
    pub fn function_prototype(&self, name: &'static str) -> Option<Value> {
        let index = function_prototype_index(name);
        if let Some(value) = self.function_prototypes.borrow()[index] {
            return Some(value);
        }
        let value = self.get(name)?;
        self.cache_barrier(value);
        self.function_prototypes.borrow_mut()[index] = Some(value);
        Some(value)
    }

    /// The realm's %Function.prototype.apply% builtin, cached after the first
    /// resolution (see the struct field).
    pub fn apply_builtin(&self) -> Option<Value> {
        if let Some(value) = self.apply_builtin.borrow().as_ref() {
            return Some(*value);
        }
        let value = self.get("%Function.prototype.apply%")?;
        self.cache_barrier(value);
        *self.apply_builtin.borrow_mut() = Some(value);
        Some(value)
    }

    /// The realm's %Function.prototype.call% builtin, cached after the first
    /// resolution (see the struct field).
    pub fn call_builtin(&self) -> Option<Value> {
        if let Some(value) = self.call_builtin.borrow().as_ref() {
            return Some(*value);
        }
        let value = self.get("%Function.prototype.call%")?;
        self.cache_barrier(value);
        *self.call_builtin.borrow_mut() = Some(value);
        Some(value)
    }

    /// The realm's %String.prototype% value, cached after the first
    /// resolution (see the struct field).
    pub fn string_prototype(&self) -> Option<Value> {
        if let Some(value) = self.string_prototype.borrow().as_ref() {
            return Some(*value);
        }
        let value = self.get("%String.prototype%")?;
        self.cache_barrier(value);
        *self.string_prototype.borrow_mut() = Some(value);
        Some(value)
    }

    /// A non-string primitive prototype intrinsic (%Number.prototype%,
    /// %Boolean.prototype%, %BigInt.prototype%, %Symbol.prototype%), cached
    /// after the first resolution (see the struct field).
    pub fn primitive_prototype(&self, name: &'static str) -> Option<Value> {
        let index = primitive_prototype_index(name);
        if let Some(value) = self.primitive_prototypes.borrow()[index] {
            return Some(value);
        }
        let value = self.get(name)?;
        self.cache_barrier(value);
        self.primitive_prototypes.borrow_mut()[index] = Some(value);
        Some(value)
    }

    /// Name the members of the realm's own builtin objects — `%Math.abs%`,
    /// `%Object.keys%`, `%get RegExp.prototype.global%`.
    ///
    /// A builtin the realm installs is a value a host's graph can hold: deno's own
    /// `00_primordials.js` copies the realm's builtins into an object it attaches
    /// to the realm it snapshots. A snapshot carries such a value by **name**,
    /// because the reading realm rebuilds the same builtins and a name is what the
    /// two realms share. The table already names the intrinsics themselves and,
    /// since Phase 6, the ten function properties of the global object; what it
    /// did not name is what those objects *hold* — every `Math` method, every
    /// prototype method the identity-chain dispatchers have no name for, every
    /// accessor getter and setter.
    ///
    /// The name is derived rather than declared at each install site because the
    /// derivation is a property of the realm's own structure, which is exactly
    /// what the writing and the reading realm share: the owner's registered name
    /// and the member key, in the convention the installs that *do* name an
    /// accessor already use (`%set Error.prototype.stack%`). A derived name that
    /// is already in the table is left alone, so a derivation can never displace a
    /// declared name and running the pass twice is a no-op.
    pub fn name_members(&self) {
        // The table is read in full before anything is written to it: `define`
        // takes the `entries` borrow.
        let owners: Vec<(Rc<str>, Value)> = self
            .entries
            .borrow()
            .iter()
            .filter(|(name, _)| !self.derived.borrow().contains(*name))
            .map(|(name, value)| (Rc::clone(name), *value))
            .collect();
        for (owner, value) in owners {
            // An intrinsic's name is `%…%`; the member's name keeps that shape
            // with the owner's own percents dropped, which is the convention the
            // installs that declare a member by name already use
            // (`%Array.prototype.at%`, `%get ArrayBuffer.prototype.byteLength%`).
            let Some(stem) = owner
                .strip_prefix('%')
                .and_then(|rest| rest.strip_suffix('%'))
            else {
                continue;
            };
            let Some(object) = member_holder(&value) else {
                continue;
            };
            let Ok(keys) = object.own_property_keys() else {
                continue;
            };
            for key in keys {
                // The member's spelling: `.abs` for a string key, and the shape the
                // installs that declare a symbol member already use
                // (`%Array.prototype[Symbol.iterator]%`) for a well-known symbol —
                // with no dot, because that is how they spell it. A key that is not
                // a well-known symbol has no such spelling and is left to the
                // install that declared it.
                let member = match &key {
                    PropertyKey::String(atom) => {
                        format!(".{}", crux::lookup(*atom).to_string_lossy())
                    }
                    PropertyKey::Symbol(symbol) => match crux::symbol::WELL_KNOWN_SYMBOLS
                        .iter()
                        .find(|name| crux::symbol::well_known(name).id == symbol.id)
                    {
                        Some(name) => format!("[Symbol.{name}]"),
                        None => continue,
                    },
                };
                let Ok(Some(property)) = object.get_own_property_key(&key) else {
                    continue;
                };
                match &property.kind {
                    PropertyKind::Data { value, .. } => {
                        self.name_member(&format!("%{stem}{member}%"), *value)
                    }
                    PropertyKind::Accessor { get, set } => {
                        if let Some(get) = get {
                            self.name_member(&format!("%get {stem}{member}%"), *get);
                        }
                        if let Some(set) = set {
                            self.name_member(&format!("%set {stem}{member}%"), *set);
                        }
                    }
                }
            }
        }
    }

    /// Register `name` for `member`, when it is a function the table does not
    /// already answer to that name with.
    ///
    /// The registration is a plain one — no dispatch handler lookup: a member's
    /// name needs none, since a call reaches its handler through the names the
    /// *install* declared (the name table is per function, so the two are the same
    /// function's names). Doing the lookup here would run twelve namespace tables
    /// over the ~355 derived names on every realm a host creates, for a miss.
    fn name_member(&self, name: &str, member: Value) {
        if member.as_function().is_none() || self.entries.borrow().contains_key(name) {
            return;
        }
        self.derived.borrow_mut().insert(Rc::from(name));
        self.name_function(name, member);
    }

    pub fn define(&self, name: &str, value: Value) {
        self.name_function(name, value);
        // Register an agent-dependent builtin's native handler so a warm
        // call dispatches in O(1) (see `builtins::array::handler_for`);
        // functions without a registered handler (prototypes, plain
        // closures, the eval hosts, the fromAsync continuations) keep the
        // intrinsic-identity chain scan.
        if let Some(function) = value.as_function()
            && let Some(handler) = crate::builtins::array::handler_for(name)
                .or_else(|| crate::builtins::regexp::handler_for(name))
                .or_else(|| crate::builtins::string::handler_for(name))
                .or_else(|| crate::builtins::number::handler_for(name))
                .or_else(|| crate::builtins::boolean::handler_for(name))
                .or_else(|| crate::builtins::bigint::handler_for(name))
                .or_else(|| crate::builtins::keyed::handler_for(name))
                .or_else(|| crate::builtins::object::handler_for(name))
                .or_else(|| crate::builtins::dataview::handler_for(name))
                .or_else(|| crate::builtins::date::handler_for(name))
                .or_else(|| crate::builtins::typed_array::handler_for(name))
                .or_else(|| crate::builtins::json::handler_for(name))
        {
            crate::function::register_builtin_handler(function.id(), handler);
        }
        // The construct-side mirror: register a constructible builtin's
        // native construct handler so a warm `new` dispatches in O(1)
        // instead of the `dispatch_construct` chain walk in
        // `construct_inner`. Functions without a registered construct
        // handler (methods, prototypes, crux-native constructors with no
        // agent-dependent chain arm) keep the chain / crux fallback.
        if let Some(function) = value.as_function()
            && let Some(ctor) = crate::builtins::array::construct_handler_for(name)
                .or_else(|| crate::builtins::regexp::construct_handler_for(name))
                .or_else(|| crate::builtins::string::construct_handler_for(name))
                .or_else(|| crate::builtins::number::construct_handler_for(name))
                .or_else(|| crate::builtins::keyed::construct_handler_for(name))
                .or_else(|| crate::builtins::object::construct_handler_for(name))
                .or_else(|| crate::builtins::dataview::construct_handler_for(name))
                .or_else(|| crate::builtins::date::construct_handler_for(name))
                .or_else(|| crate::builtins::array_buffer::construct_handler_for(name))
        {
            crate::function::register_builtin_ctor(function.id(), ctor);
        }
    }

    /// A plain name registration: the barrier, the name, and the function's name
    /// list — everything `define` does *except* the dispatch handler lookups,
    /// which only a name an install declares can need (see `name_member`).
    fn name_function(&self, name: &str, value: Value) {
        self.cache_barrier(value);
        let key: Rc<str> = Rc::from(name);
        self.entries.borrow_mut().insert(Rc::clone(&key), value);
        if let Some(function) = value.as_function() {
            let mut names = self.names.borrow_mut();
            match names.get_mut(&function.id()) {
                Some(aliases) => Rc::make_mut(aliases).push(Rc::clone(&key)),
                None => {
                    names.insert(function.id(), Rc::new(vec![Rc::clone(&key)]));
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }

    /// Whether the registry holds `value` — the owning-realm lookup for
    /// cross-realm builtin calls (`$262.createRealm` fixtures).
    pub fn contains(&self, value: &Value) -> bool {
        self.entries.borrow().values().any(|entry| entry == value)
    }

    /// Every registered intrinsic value, for post-install linking.
    pub fn entries(&self) -> Vec<Value> {
        self.entries.borrow().values().cloned().collect()
    }
}

fn as_object(value: &Value) -> Option<Handle<JsObject>> {
    value.as_object()
}

/// InitializeHostDefinedRealm (spec 9.3.4): CreateIntrinsics, the global
/// object, NewGlobalEnvironment, and SetDefaultGlobalBindings. The caller
/// pushes the bootstrap execution context.
/// The object a value's own members live on: an object's own box, or a
/// function's object part — a builtin constructor's statics are properties of
/// the function.
fn member_holder(value: &Value) -> Option<Handle<JsObject>> {
    if let ValueKind::Function(function) = value.kind() {
        return function.object.handle();
    }
    value.as_object()
}

pub fn initialize_host_defined_realm(agent: &Agent) -> Result<Handle<Realm>, JsError> {
    initialize_host_defined_realm_with_global(agent, None)
}

/// The same, with an optional host-defined exotic on the global object (spec
/// 9.3.4's InitializeHostDefinedRealm, which leaves the global's internal
/// methods to the host). The ops run those methods with ordinary fallback, so a
/// realm whose host supplies none is exactly the realm above.
pub fn initialize_host_defined_realm_with_global(
    agent: &Agent,
    global_ops: Option<Rc<dyn HostOps>>,
) -> Result<Handle<Realm>, JsError> {
    let intrinsics = Intrinsics::default();
    // The global object's prototype is %Object.prototype% once the intrinsic
    // table is populated (Phase 5+); until then it is null.
    let prototype = intrinsics
        .get("%Object.prototype%")
        .and_then(|v| as_object(&v));
    let global = match global_ops {
        Some(ops) => JsObject::host_object_create(ops, prototype),
        None => JsObject::ordinary_object_create(prototype),
    };
    let global_env = new_global_environment(global, global);
    let realm = Handle::new(Realm {
        agent_signifier: agent.signifier,
        intrinsics,
        global_object: global,
        global_env,
        promise_hooks: RefCell::new(PromiseHooks::default()),
        loaded_modules: RefCell::new(std::collections::HashMap::new()),
        microtask_queue: std::cell::Cell::new(None),
    });
    // Root the realm from the moment its box exists. `Agent::realms` is the
    // realm's permanent root (it is only ever pushed, never popped or
    // cleared), and the function records registered below no longer carry a
    // `realm` edge of their own (see `EcmaFunction`'s trace) — so this push is
    // what keeps the realm alive through `set_default_global_bindings` under an
    // allocation-collecting build, instead of a Rust local surviving the
    // conservative scan.
    agent.realms.borrow_mut().push(realm);
    agent.realm_count.set(agent.realm_count.get() + 1);
    realm.intrinsics.set_owner(realm);
    set_default_global_bindings(&realm)?;
    Ok(realm)
}

/// SetDefaultGlobalBindings (spec 9.3.5). Phase 4 installs the global
/// object's value properties (spec sec-value-properties-of-the-global-object);
/// the function and constructor properties arrive with their built-ins
/// (Phase 8+).
fn set_default_global_bindings(realm: &Handle<Realm>) -> Result<(), JsError> {
    let global = &realm.global_object;
    global.define_property_or_throw(
        &JsString::from_utf8("globalThis"),
        &PropertyDescriptor {
            value: Some(Value::Object(*global)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    for (name, value) in [
        ("Infinity", Value::Number(f64::INFINITY)),
        ("NaN", Value::Number(f64::NAN)),
        ("undefined", Value::Undefined),
    ] {
        global.define_property_or_throw(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(value),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(false),
            },
        )?;
    }
    // %eval% (spec 19.2.1): a global whose identity the call evaluator
    // recognizes to perform direct and indirect eval (sec 13.3.6.1). Its
    // native body is a placeholder; dispatch happens before it runs.
    let eval_func = Function::create_builtin(
        Some(JsString::from_utf8("eval")),
        1,
        Box::new(|_, _| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "eval must be called through the evaluator".into(),
            ))
        }),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define("%eval%", Value::Function(eval_func));
    global.define_property_or_throw(
        &JsString::from_utf8("eval"),
        &PropertyDescriptor {
            value: Some(Value::Function(eval_func)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    crate::builtins::object::install(realm)?;
    // The global object's [[Prototype]] is implementation-defined but the
    // host-standard shape (browsers, Node, the test262 harness) inherits
    // %Object.prototype% so `globalThis.toString` and friends resolve.
    if let Some(object_proto) = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value))
    {
        realm.global_object.set_prototype_of(Some(object_proto))?;
    }
    crate::builtins::function::install(realm)?;
    crate::builtins::array::install(realm)?;
    crate::builtins::typed_array::install(realm)?;
    crate::builtins::boolean::install(realm)?;
    crate::builtins::bigint::install(realm)?;
    crate::builtins::date::install(realm)?;
    crate::builtins::symbol::install(realm)?;
    crate::builtins::error::install(realm)?;
    // The WebAssembly JS-API namespace ships with the default-on `wasm`
    // feature; without it the global simply is not installed.
    #[cfg(feature = "wasm")]
    crate::builtins::wasm::install(realm)?;
    crate::builtins::global::install(realm)?;
    crate::builtins::math::install(realm)?;
    crate::builtins::number::install(realm)?;
    crate::builtins::string::install(realm)?;
    crate::builtins::regexp::install(realm)?;
    crate::builtins::keyed::install(realm)?;
    crate::builtins::array_buffer::install(realm)?;
    crate::builtins::dataview::install(realm)?;
    crate::builtins::atomics::install(realm)?;
    crate::builtins::json::install(realm)?;
    crate::builtins::promise::install(realm)?;
    crate::builtins::module_source::install(realm)?;
    crate::generator::install(realm)?;
    crate::builtins::async_iterator::install(realm)?;
    crate::async_generator::install(realm)?;
    crate::builtins::async_function::install(realm)?;
    crate::builtins::weakref::install(realm)?;
    crate::builtins::iterator::install(realm)?;
    crate::builtins::disposable::install(realm)?;
    crate::builtins::proxy::install(realm)?;
    crate::builtins::reflect::install(realm)?;
    crate::builtins::temporal::install(realm)?;
    crate::builtins::intl::install(realm)?;
    // ES2022+: every built-in iterator prototype object inherits
    // %Iterator.prototype%, which installs after them. Re-parent them now
    // that the whole table is populated.
    if let Some(iterator_proto) = realm
        .intrinsics
        .get("%Iterator.prototype%")
        .and_then(|value| as_object(&value))
    {
        for name in [
            "%Generator.prototype%",
            "%ArrayIteratorPrototype%",
            "%StringIteratorPrototype%",
            "%MapIteratorPrototype%",
            "%SetIteratorPrototype%",
            "%RegExpStringIteratorPrototype%",
        ] {
            if let Some(proto) = realm
                .intrinsics
                .get(name)
                .and_then(|value| as_object(&value))
            {
                proto.set_prototype_of(Some(iterator_proto))?;
            }
        }
    }
    // spec 10.3.1: every built-in function object's [[Prototype]] is
    // %Function.prototype%. Link all intrinsic-registered functions now that
    // the table is full; %Function.prototype% itself keeps %Object.prototype%
    // (setting its own proto would be a cycle, which set_prototype_of
    // rejects), and installs that set a custom [[Prototype]] (the TypedArray
    // kind constructors inherit %TypedArray%) are left alone.
    let function_proto = match realm.intrinsics.get("%Function.prototype%") {
        Some(value) => match value.kind() {
            ValueKind::Function(function) => function.object.handle(),
            _ => None,
        },
        None => None,
    };
    if let Some(function_proto) = function_proto {
        for value in realm.intrinsics.entries() {
            if let ValueKind::Function(function) = value.kind()
                && let Some(object) = function.object.handle()
                && object.get_prototype_of()?.is_none()
            {
                object.set_prototype_of(Some(function_proto))?;
            }
        }
    }
    // The last thing the table gets: a name for every function the realm's own
    // builtin objects hold, so a host's graph can hold a builtin the same way it
    // holds an intrinsic — by name, which the reading realm resolves from its own
    // installs (see `Intrinsics::name_members`).
    realm.intrinsics.name_members();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    #[test]
    fn intrinsics_store_and_lookup_percent_names() {
        let intrinsics = Intrinsics::default();
        assert!(intrinsics.is_empty());
        assert!(intrinsics.get("%Object.prototype%").is_none());
        intrinsics.define("%Object.prototype%", Value::Undefined);
        assert!(!intrinsics.is_empty());
        assert_eq!(intrinsics.get("%Object.prototype%"), Some(Value::Undefined));
    }

    /// A realm whose host asks for a host-defined global object gets one: the
    /// global's internal methods are the host's, with ordinary fallback. This is
    /// the mechanism a V8 named property handler rides on, so the test checks
    /// the three methods one needs — `[[GetOwnProperty]]` (the descriptor
    /// callback), `[[Set]]` (the setter) and `[[DefineOwnProperty]]` (the
    /// definer) — and that answering `None` really leaves the ordinary method
    /// running.
    #[test]
    fn a_realms_global_object_can_be_host_defined() {
        #[derive(Debug)]
        struct Recording(std::rc::Rc<RefCell<Vec<&'static str>>>);

        impl HostOps for Recording {
            fn get_own_property(
                &self,
                _object: &JsObject,
                _key: &PropertyKey,
            ) -> Option<Result<crux::object::Property, JsError>> {
                self.0.borrow_mut().push("descriptor");
                None
            }

            fn set(
                &self,
                _object: &JsObject,
                _key: &PropertyKey,
                _value: &Value,
                _receiver: &Value,
                _throw: bool,
            ) -> Option<Result<bool, JsError>> {
                self.0.borrow_mut().push("setter");
                None
            }

            fn define_property(
                &self,
                _object: &JsObject,
                _key: &PropertyKey,
                _desc: &PropertyDescriptor,
            ) -> Option<Result<bool, JsError>> {
                self.0.borrow_mut().push("definer");
                None
            }
        }

        let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
        let mut agent = Agent::new();
        agent
            .initialize_host_defined_realm_with_global(Some(std::rc::Rc::new(Recording(
                seen.clone(),
            ))))
            .unwrap();
        agent
            .run_script(
                "Object.defineProperty(globalThis, 'key', { value: 9, enumerable: true, configurable: true, writable: true });\
                 globalThis.other = 1;\
                 Object.getOwnPropertyDescriptor(globalThis, 'key');",
            )
            .unwrap();
        let calls = seen.borrow().clone();
        for call in ["definer", "setter", "descriptor"] {
            assert!(
                calls.contains(&call),
                "the host's {call} was not consulted: {calls:?}"
            );
        }
        // Every answer was `None` — not intercepted — so the ordinary methods
        // still ran and both properties are where the script put them.
        assert_eq!(
            agent
                .run_script("globalThis.other === 1 && globalThis.key === 9")
                .unwrap(),
            Value::Boolean(true),
            "a host op that answers `None` leaves the ordinary method running"
        );
    }

    /// A builtin the realm installs on one of its own objects is **nameable** by
    /// the table — the name a snapshot writes such a value as — derived from the
    /// owner's own name and the member key, in the convention the installs that
    /// declare a member already use.
    #[test]
    fn a_realm_names_the_members_of_its_own_builtin_objects() {
        let agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        let math = realm
            .global_object
            .get(&JsString::from_utf8("Math"))
            .unwrap()
            .as_object()
            .expect("%Math% is an object");
        let abs = math.get(&JsString::from_utf8("abs")).unwrap();
        assert!(abs.as_function().is_some(), "Math.abs is a function");
        assert_eq!(
            realm.intrinsics.get("%Math.abs%"),
            Some(abs),
            "a namespace object's method is named by the object's name and the key"
        );

        // The two halves of an accessor are two names, distinguished the way the
        // installs that declare one distinguish them (`%get %TypedArray%…`), and a
        // member the table already answers to is left alone rather than renamed.
        let getter = Function::create_builtin(
            Some(JsString::from_utf8("get x")),
            0,
            Box::new(|_, _| Ok(Value::Number(1.0))),
            None,
            None,
        )
        .unwrap();
        math.define_property(
            &JsString::from_utf8("x"),
            &PropertyDescriptor {
                value: None,
                writable: None,
                get: Some(Value::Function(getter)),
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )
        .unwrap();
        realm.intrinsics.name_members();
        assert_eq!(
            realm.intrinsics.get("%get Math.x%"),
            Some(Value::Function(getter)),
            "an accessor's getter is named with the `get` prefix convention"
        );

        // A derived name is not an owner in turn. `%Array.prototype.constructor%`
        // is one pass's spelling of `%Array%`; if it were scanned as an owner the
        // next pass would mint `%Array.prototype.constructor.from%` beside the
        // declared `%Array.from%`, and no pass would ever be the last.
        assert_eq!(
            realm.intrinsics.get("%Array.prototype.constructor.from%"),
            None,
            "a derived name never becomes an owner, so the pass is idempotent"
        );
        assert!(realm.intrinsics.get("%Array.from%").is_some());
        assert_eq!(
            realm.intrinsics.get("%Math.abs%"),
            Some(abs),
            "and a name already in the table is not displaced"
        );
    }

    #[test]
    fn host_defined_realm_installs_value_properties() {
        let agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        let global = &realm.global_object;
        // globalThis points back at the global object...
        assert_eq!(
            global.get(&JsString::from_utf8("globalThis")).unwrap(),
            Value::Object(*global)
        );
        // ...and the non-configurable value properties exist.
        assert_eq!(
            global.get(&JsString::from_utf8("Infinity")).unwrap(),
            Value::Number(f64::INFINITY)
        );
        assert!(matches!(
            global.get(&JsString::from_utf8("NaN")).unwrap().kind(),
            ValueKind::Number(n) if n.is_nan()
        ));
        assert_eq!(
            global.get(&JsString::from_utf8("undefined")).unwrap(),
            Value::Undefined
        );
        let infinity = global
            .get_own_property(&JsString::from_utf8("Infinity"))
            .unwrap()
            .unwrap();
        assert_eq!(infinity.writable(), Some(false));
        assert!(!infinity.enumerable && !infinity.configurable);
        // The global environment is reachable and supplies `this`.
        assert_eq!(
            realm.global_env.get_this_binding().unwrap(),
            Value::Object(*global)
        );
        // Binding an identifier through the global env works end to end.
        realm
            .global_env
            .create_mutable_binding(&JsString::from_utf8("x"), false)
            .unwrap();
        realm
            .global_env
            .initialize_binding(&JsString::from_utf8("x"), Value::Number(42.0))
            .unwrap();
        assert_eq!(
            realm
                .global_env
                .get_binding_value(&JsString::from_utf8("x"), true)
                .unwrap(),
            Value::Number(42.0)
        );
    }

    #[test]
    fn global_property_completeness_check() {
        // The global property list installed so far (spec 19.1-19.3, Phase 8
        // slice). Every entry must be present, writable, non-enumerable, and
        // configurable (except the non-configurable value properties).
        let agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        let global = &realm.global_object;
        let expected = [
            "globalThis",
            "Infinity",
            "NaN",
            "undefined",
            "eval",
            "isFinite",
            "isNaN",
            "parseFloat",
            "parseInt",
            "encodeURI",
            "encodeURIComponent",
            "decodeURI",
            "decodeURIComponent",
            "queueMicrotask",
            "Object",
            "Function",
            "Boolean",
            "Symbol",
            "Error",
            "EvalError",
            "RangeError",
            "ReferenceError",
            "SyntaxError",
            "TypeError",
            "URIError",
            "AggregateError",
            "SuppressedError",
            "Promise",
            "Map",
            "Set",
            "WeakMap",
            "WeakSet",
        ];
        for name in expected {
            assert!(
                global.has_own_property(&JsString::from_utf8(name)).unwrap(),
                "missing global property {name}"
            );
        }
        // Function properties are non-enumerable and configurable.
        let descriptor = global
            .get_own_property(&JsString::from_utf8("parseInt"))
            .unwrap()
            .unwrap();
        assert!(!descriptor.enumerable && descriptor.configurable);
        assert_eq!(descriptor.writable(), Some(true));
    }
}
