//! Agents (spec 9.7) and the surrounding-agent operations.
//!
//! The agent owns the execution context stack and the job queues; its
//! record fields ([[]] names below) are the Agent Record fields of the
//! spec 9.7 table. Single-threaded: [[CanBlock]] is false, so
//! AgentCanSuspend() is false.

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::heap::{GcAny, Trace};
use crux::string::JsString;
use crux::symbol::Symbol;
use crux::value::Value;
use crux::value::ValueKind;

use crate::context::ExecutionContext;
use crate::host::HostHooks;
use crate::job::Job;
use crate::realm::{Realm, initialize_host_defined_realm_with_global};

static NEXT_AGENT_ID: AtomicU64 = AtomicU64::new(1);

// The closure region of the job currently running (GC-2): its opaque
// captures must be conservatively scanned alongside the queued jobs'.
thread_local! {
    static RUNNING_JOB_REGION: std::cell::RefCell<Option<(*const u8, usize)>> =
        const { std::cell::RefCell::new(None) };
}

/// One entry of a Map/WeakMap `[[*Data]]` List: `None` is the ~empty~
/// (deleted) slot, `Some((key, value))` a live entry.
pub type MapEntry = Option<(Value, Value)>;

/// One element of a Set/WeakSet `[[*Data]]` List.
pub type SetEntry = Option<Value>;

/// The key index of a Map/Set collection: the SameValue-consistent hash
/// word of each LIVE key -> the slot of its entry in `entries`. The words
/// never reference a GC box (numbers fold to bits, strings hash content,
/// object keys use their stable id), so the index needs no tracing; every
/// mutation (set/add/delete/clear) keeps it exactly over the live slots, and
/// a deleted slot drops its rows so a row can never dangle. A word collision
/// between distinct keys is resolved by the caller's exact SameValue check
/// on the slot (with a linear-scan fallback), so the index is a pure
/// accelerator over the tombstoned `entries` List.
pub type KeyedIndex =
    std::collections::HashMap<u64, usize, std::hash::BuildHasherDefault<MixHasher>>;

/// A cheap avalanche hasher for the already-64-bit key words of
/// [`KeyedIndex`]. Std's SipHash costs ~20ns per Map/Set operation for no
/// distribution benefit (the word is already a hash), but the raw word cannot
/// be used directly: a small integer's f64 bit pattern has all-zero low bits,
/// so identity hashing would cluster every numeric key into one bucket.
/// splitmix64's finalizer is a handful of instructions and spreads both.
#[derive(Default)]
pub struct MixHasher(u64);

impl std::hash::Hasher for MixHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut buf = [0u8; 8];
        for (dst, src) in buf.iter_mut().zip(bytes) {
            *dst = *src;
        }
        self.write_u64(u64::from_ne_bytes(buf));
    }
    fn write_u64(&mut self, i: u64) {
        let mut z = i.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        self.0 = z ^ (z >> 31);
    }
    fn write_u32(&mut self, i: u32) {
        self.write_u64(i as u64);
    }
    fn write_usize(&mut self, i: usize) {
        self.write_u64(i as u64);
    }
}

/// The [[MapData]] List of a Map instance (spec 24.1.1; `None` is a deleted
/// ~empty~ slot kept so suspended iterators keep working) plus its key
/// index. The index maps the SameValue-consistent hash word of each live key
/// to its slot; because every live key owns a row (except under a genuine
/// 64-bit word collision, tracked by `collided`), a word with no row is an
/// authoritative miss and a delete drops its row in O(1).
#[derive(Default)]
pub struct MapCollection {
    pub entries: Vec<MapEntry>,
    pub index: KeyedIndex,
    /// Whether two live keys ever shared a hash word (a 64-bit collision):
    /// while set, the single-slot index cannot shadow one of them, so the
    /// ops fall back to the exact scan instead of trusting the row.
    pub collided: bool,
}

/// The `Array.fromAsync` continuation state (spec 23.1.2.4.1): the handler's
/// state and whether the handler is the rejection one.
pub(crate) type FromAsyncContinuation = (
    std::rc::Rc<RefCell<crate::builtins::array::FromAsyncState>>,
    bool,
);

/// The record of a Map or Set iterator: the iterated collection (or `None`
/// once iteration finished), the next index, and the iteration kind's code
/// (spec 24.1.6.1).
pub type IterRecord = (Option<Value>, usize, u8);

/// The record of a `%RegExpStringIterator%`: the RegExp, the string, the
/// `global` and `fullUnicode` flags, and the done flag (spec 22.2.8).
pub type RegexpStringIterRecord = (Value, crux::string::JsString, bool, bool, bool);

/// The [[SetData]] of a Set instance (spec 24.2.1) plus its element index.
#[derive(Default)]
pub struct SetCollection {
    pub entries: Vec<SetEntry>,
    pub index: KeyedIndex,
    pub collided: bool,
}

impl Trace for MapCollection {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.entries.trace(visit);
    }
}

impl Trace for SetCollection {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.entries.trace(visit);
    }
}

/// The GC box address of a heap value, `None` for doubles and the non-heap
/// tags (GC-3: the weak-table compaction and ephemeron registration identify
/// entries by their key's box).
pub fn value_box_addr(value: &Value) -> Option<crux::heap::GcAny> {
    match value.kind() {
        ValueKind::Object(object) => Some(object.as_any()),
        ValueKind::Function(function) => Some(function.as_any()),
        ValueKind::String(text) => Some(text.as_any()),
        ValueKind::Symbol(symbol) => Some(symbol.as_any()),
        ValueKind::BigInt(bigint) => Some(bigint.as_any()),
        _ => None,
    }
}

/// The surrounding agent: the execution context stack, the job queues, and
/// the Agent Record fields of spec 9.7.
#[derive(Debug)]
/// A no-op hasher for the identity-keyed tables: the keys are already-
/// unique u64 ids (function/object identity), so hashing them with SipHash
/// — std's default — costs ~20ns per `Call` for zero distribution benefit.
/// The HashMap still probes and compares keys, so collisions are handled
/// exactly as before.
#[derive(Default)]
pub struct IdentityHasher(u64);

impl std::hash::Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut buf = [0u8; 8];
        for (dst, src) in buf.iter_mut().zip(bytes) {
            *dst = *src;
        }
        self.0 = u64::from_ne_bytes(buf);
    }
    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }
    fn write_u32(&mut self, i: u32) {
        self.0 = i as u64;
    }
    fn write_usize(&mut self, i: usize) {
        self.0 = i as u64;
    }
}

/// An id-keyed state map: the key is an object's or a function's identity — a
/// sequential `u64` from the id counter — so the default SipHash is pure
/// overhead on every lookup. The same reasoning (and the same hasher) already
/// backs `ecma_functions`, which is on the function-call path.
///
/// Do NOT use this for a key that is not already well distributed: a map whose
/// key is a raw `f64::to_bits()` (numbers below 1 have low bits clear) needs
/// real mixing, which `IdentityHasher` does not do.
pub type IdMap<V> =
    std::collections::HashMap<u64, V, std::hash::BuildHasherDefault<IdentityHasher>>;

/// The cached "the Array-iteration infrastructure is stock" verdict (Cut
/// 24): %Array.prototype%'s own @@iterator is the intrinsic,
/// %ArrayIteratorPrototype% has the stock `next`, and no `return` on the
/// AIP → %Object.prototype% chain. The handles are kept so a probe can
/// re-read the generations; a structural mutation bumps one and re-resolves
/// the full check. A VALUE write to AIP's own `next` (a warm store that
/// does not bump under the L1c record discipline) is caught by the probe's
/// oracle check: `aip_next_vindex` is the (AIP id, "next") member value
/// cell slot, which resolve warms with the stock `aip_next` and every warm
/// store to AIP.next fronts, so a probe compares the live cell value to the
/// recorded stock value.
#[derive(Clone, Copy)]
pub(crate) struct ForOfFastVerdict {
    pub array_proto: (u64, u32),
    pub aip: (u64, u32),
    pub aip_handle: Handle<crux::object::JsObject>,
    pub object_proto: (u64, u32),
    pub object_proto_handle: Handle<crux::object::JsObject>,
    /// The stock `next` function the verdict verified on AIP.
    pub aip_next: Value,
    /// The member-value-cell slot the probe re-reads AIP's `next` through.
    pub aip_next_vindex: usize,
}

impl Trace for ForOfFastVerdict {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.aip_handle.trace(visit);
        self.object_proto_handle.trace(visit);
        self.aip_next.trace(visit);
    }
}

/// The cached "the Array species machinery is stock" verdict: this realm's
/// %Array.prototype% has an own data `constructor` equal to this realm's
/// %Array%, and %Array% carries the stock @@species accessor (whose getter is
/// the SPECIES intrinsic, returning `this`). A stock-Array species create
/// therefore resolves `constructor` -> %Array% and @@species -> %Array%, i.e.
/// exactly an ArrayCreate with that %Array.prototype%. Re-validated by
/// generation: a structural mutation of either shared object (constructor
/// redefined/deleted/replaced, @@species redefined, %Array% re-frozen) bumps
/// one and re-resolves. A VALUE write to %Array.prototype%.constructor that
/// does not bump (the compiled member-store path) is caught by the value
/// oracle: `ctor_vindex` is the (%Array.prototype% id, "constructor") member
/// value cell slot, warmed at resolve with the %Array% value and refreshed by
/// every warm store, so a probe compares the live cell value to the recorded
/// %Array% value.
#[derive(Clone, Copy)]
pub(crate) struct ArraySpeciesVerdict {
    pub array_proto: (u64, u32),
    /// The %Array% function's own data: (object-part id, generation). A
    /// structural change to %Array%'s @@species property bumps the object
    /// part's generation; an in-place VALUE write cannot touch an accessor.
    pub array_ctor_object: (u64, u32),
    /// The %Array% function value the oracle cell must still hold (also what
    /// the species resolution constructs).
    pub array_ctor_value: Value,
    /// The member-value-cell slot the probe re-reads
    /// (%Array.prototype%, "constructor") through.
    pub ctor_vindex: usize,
}

impl Trace for ArraySpeciesVerdict {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.array_ctor_value.trace(visit);
    }
}

/// The cached "the %Array.prototype% -> %Object.prototype% chain carries no
/// own @@isConcatSpreadable" verdict: a plain Array on that chain spreads in
/// `concat` without the per-element @@isConcatSpreadable symbol read. The two
/// shared objects' generations gate later defines/deletes (an absent own
/// property can only appear by a define, which bumps; symbol-keyed stores
/// never take the compiled no-bump member-store path, which is name-keyed).
#[derive(Clone, Copy)]
pub(crate) struct ConcatSpreadVerdict {
    pub array_proto: (u64, u32),
    pub object_proto: (u64, u32),
    pub object_proto_handle: Handle<crux::object::JsObject>,
}

impl Trace for ConcatSpreadVerdict {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.object_proto_handle.trace(visit);
    }
}

// Every live `Agent` on this thread that has run JavaScript, as
// `(signifier, agent, roots)` triples. The crux heap is thread-local and shared
// by every agent on the thread, but a collection roots only the collecting
// agent's own tables, so a nested or sibling agent's boxes — reachable only
// through its Rust-heap tables, which the conservative stack scan cannot
// see — would be swept. An agent is registered the first time it enters a
// `with_agent` window and removed in `Drop`; an agent that moves between
// runs re-registers its new address under the same signifier.
//
// `roots` is the agent's [`AgentRoots`] allocation, and it is the only thing a
// collection reaches: every traced container and every collection-time reaper
// lives there, so the collector never forms a `&Agent` — the tag an
// interpreter frame's `&mut Agent` carries is never invalidated by a
// collection, which is what M-1 was about.
/// The live agents registered for the entered heap, as `(signifier, roots)`.
///
/// The registry belongs to the heap, not the thread: a collection roots the
/// agents of the heap it collects, so an isolate that moves threads takes its
/// agents' roots with it and leaves no stale entry behind on the old one. A
/// heap shared by several agents — this thread's own, which a bare `Agent`
/// uses — still roots all of them, which is the whole point: an agent's state
/// is reachable only from its own Rust-heap tables, and the conservative stack
/// scan cannot see those.
fn with_live_agents<R>(body: impl FnOnce(&RefCell<Vec<(u64, *const AgentRoots)>>) -> R) -> R {
    crux::heap::with_heap_cell(|| RefCell::new(Vec::new()), body)
}

/// The `with_agent` enter notifier (installed by [`crate::function::ensure_ecma_hook`]):
/// register the agent as a GC root. Fires only on genuine window transitions.
pub(crate) fn live_agent_entered(agent: *mut ()) {
    let agent = agent as *const Agent;
    // SAFETY: the notifier runs while `with_agent` has just recorded `agent`
    // as current, so it is a live `Agent` for the whole window.
    let (signifier, roots) = unsafe { ((*agent).signifier, Rc::as_ptr(&(*agent).roots)) };
    let entered = crux::heap::isolate_entered();
    let registry = with_live_agents(|cell| {
        let mut live = cell.borrow_mut();
        // An agent entered again re-registers; the entry left behind names the
        // address it had before, which is not where it runs now.
        live.retain(|&(id, _)| id != signifier);
        live.push((signifier, roots));
        cell as *const _
    });
    // An isolate's heap owns the registry and outlives the agent
    // (`IsolateInner` drops its engine before its heap), so `Drop` can release
    // through this pointer with no heap entered. This thread's own registry
    // dies with the thread, so that case is resolved at drop time instead.
    // SAFETY: the agent is live for the window, and the field is a `Cell`.
    unsafe {
        let roots = &(*agent).roots;
        roots
            .live_registry
            .set(if entered { registry } else { std::ptr::null() })
    };
}

/// The registered live agents' root containers (see [`with_live_agents`]).
pub(crate) fn live_agent_roots() -> Vec<*const AgentRoots> {
    with_live_agents(|live| live.borrow().iter().map(|&(_, roots)| roots).collect())
}

/// Whether `agent`'s roots are registered as a live agent's (tests).
#[cfg(test)]
pub(crate) fn is_live_agent(agent: *const Agent) -> bool {
    // SAFETY: the caller passes a live agent (the embed tests check a pointer
    // they still own).
    let roots = unsafe { Rc::as_ptr(&(*agent).roots) };
    with_live_agents(|live| {
        live.borrow()
            .iter()
            .any(|&(_, registered)| registered == roots)
    })
}

/// Unregister a dropped agent (by its unique, never-reused signifier).
fn unregister_live_agent(roots: &Rc<AgentRoots>, signifier: u64) {
    let registry = roots.live_registry.get();
    if !registry.is_null() {
        // SAFETY: the isolate's heap owns the registry and outlives the agent.
        let live = unsafe { &*registry };
        if let Ok(mut live) = live.try_borrow_mut() {
            live.retain(|&(id, _)| id != signifier);
        }
    }
    // An agent that ran once with no isolate entered registered in this
    // thread's own registry first, and that entry is a raw pointer: it must not
    // outlive the agent even though the isolate's registry is the recorded one.
    // The registry is gone only when the thread is, and a drop during teardown
    // must not panic.
    let _ = crux::heap::with_own_heap_cell::<RefCell<Vec<(u64, *const AgentRoots)>>, _>(
        || RefCell::new(Vec::new()),
        |live| live.borrow_mut().retain(|&(id, _)| id != signifier),
    );
}

/// The host's near-heap-limit callback
/// (v8::Isolate::AddNearHeapLimitCallback): the limit that was reached and the
/// one the host first asked for go in, the limit to continue with comes out.
/// Boxed because the agent owns it; the host's own `data` pointer lives inside
/// the closure the bridge builds around it.
pub type NearHeapLimitCallback = Box<dyn FnMut(usize, usize) -> usize>;

/// The captured stack of an Error instance: the string the
/// `%Error.prototype.stack%` accessor answers with, and the frames it was
/// rendered from.
///
/// The frames are kept as well as the string because a host may want to format
/// the trace itself (`Isolate::SetPrepareStackTraceCallback`, which is how V8
/// hands a host's formatter the error and its call sites): the engine captures
/// the frames either way, so keeping them costs the capture nothing and is the
/// only way the accessor can answer the hook on a read that happens long after
/// the stack has moved on.
#[derive(Clone)]
pub struct ErrorStack {
    /// The rendered V8-style trace — the header plus one line per frame — which
    /// is what a host with no formatter reads.
    pub rendered: crux::string::JsString,
    /// The frames the trace was rendered from, innermost first.
    pub frames: Vec<crate::api::StackFrame>,
}

impl crux::heap::Trace for ErrorStack {
    fn trace(&self, visit: &mut dyn FnMut(crux::heap::GcAny)) {
        self.rendered.trace(visit);
        // The frames carry text, numbers and flags rather than values (a frame
        // has no handle to anything), so they hold no edge to trace.
    }
}

/// The key of a cached SCRIPT/EVAL body: the exact source text plus
/// (strict, fast_script, jsx) — see [`AgentRoots::script_bodies`].
pub(crate) type ScriptBodyKey = (crux::JsString, bool, bool, bool);

/// One leaf-inline cache cell: the function id it was filled for plus the
/// record body's hot path needs.
pub(crate) type LeafCacheCell = Option<(u64, crate::ir::LeafEntry)>;

/// The agent's execution-context stack (spec 9.4).
///
/// It lives in [`AgentRoots`] with the other containers the collector reads, so
/// every operation takes `&self`: the interpreter pushes and pops contexts from
/// frames that hold a `&mut Agent`, and a `&mut` receiver would make the
/// collector's read the second exclusive access the M-1 note describes.
///
/// [`last`](Self::last) and [`last_mut`](Self::last_mut) hand out a guard rather
/// than a reference, because a pushed or popped context cannot be pointed at
/// from outside the cell. Holding one across a push or pop panics, so a guard's
/// life has to stay inside one operation.
pub(crate) struct ContextStack(RefCell<Vec<ExecutionContext>>);

impl ContextStack {
    fn new() -> ContextStack {
        ContextStack(RefCell::new(Vec::new()))
    }

    pub(crate) fn push(&self, context: ExecutionContext) {
        self.0.borrow_mut().push(context);
    }

    pub(crate) fn pop(&self) -> Option<ExecutionContext> {
        self.0.borrow_mut().pop()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.borrow().len()
    }

    /// The running context: the top of the stack.
    pub(crate) fn last(&self) -> Option<Ref<'_, ExecutionContext>> {
        Ref::filter_map(self.0.borrow(), |stack| stack.last()).ok()
    }

    pub(crate) fn last_mut(&self) -> Option<RefMut<'_, ExecutionContext>> {
        RefMut::filter_map(self.0.borrow_mut(), |stack| stack.last_mut()).ok()
    }

    /// Read the whole stack under its guard. For the walks that need every
    /// context (a stack trace, the active script), which cannot hold a guard's
    /// lifetime across their own work.
    pub(crate) fn with<T>(&self, read: impl FnOnce(&[ExecutionContext]) -> T) -> T {
        read(&self.0.borrow())
    }
}

impl Trace for ContextStack {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.0.trace(visit);
    }
}

/// The containers the collector reads.
///
/// They live in their own allocation, reached through `Rc`, and every field is
/// interior-mutable, because both the interpreter and the collector reach them
/// through `&self`: a collection can run inside any allocation — including one
/// made by a host callback the interpreter entered through `&mut Agent` — and
/// the collector must read these tables without that `&mut Agent` asserting
/// exclusive access to their memory. Keeping them in `Agent` would make every
/// collection a second access to memory a live `&mut Agent` already covers (see
/// the M-1 note in `.notes/security-audit.md`).
///
/// This is slice 1 of that conversion: the job queues, the promise tables and
/// the generator tables. The rest of `Agent::trace_roots`'s fields follow in
/// later slices.
pub(crate) struct AgentRoots {
    /// The registry this agent registered in, for `Drop` to release the entry
    /// when no heap is entered. Null when it registered in this thread's own
    /// registry (see [`live_agent_entered`]).
    live_registry: std::cell::Cell<*const RefCell<Vec<(u64, *const AgentRoots)>>>,
    /// The execution-context stack (spec 9.4); see [`ContextStack`].
    pub(crate) execution_context_stack: ContextStack,
    pub(crate) promise_jobs: RefCell<VecDeque<Job>>,
    pub(crate) generic_jobs: RefCell<VecDeque<Job>>,
    pub(crate) timeout_jobs: RefCell<VecDeque<(u64, Job)>>,
    /// The host's own microtask queues, one per queue it made
    /// (`Agent::new_microtask_queue`). An entry is `None` once the host released
    /// the queue, and the id stays valid so a stale one is a no-op rather than
    /// somebody else's queue.
    pub(crate) microtask_queues: RefCell<Vec<Option<MicrotaskQueue>>>,
    /// [[KeptAlive]]: objects/symbols kept alive until the end of the current
    /// Job (WeakRef, Phase 13).
    pub(crate) kept_alive: RefCell<Vec<Value>>,
    /// The Promise Records keyed by promise-object identity (spec 27.2.1).
    /// Boxed behind an `Rc` so a reader can take the record and release the
    /// table's borrow before running anything that may allocate.
    pub(crate) promises:
        RefCell<std::collections::HashMap<u64, Rc<RefCell<crate::promise::PromiseData>>>>,
    /// The resolving functions created by CreateResolvingFunctions, keyed by
    /// function identity (spec 27.2.1.3).
    pub(crate) promise_resolvers:
        RefCell<std::collections::HashMap<u64, Rc<RefCell<crate::promise::ResolverData>>>>,
    /// Per-element handlers created by the Promise combinators (all/
    /// allSettled/any), keyed by function identity.
    pub(crate) promise_compound: RefCell<
        std::collections::HashMap<u64, Rc<RefCell<crate::builtins::promise::CompoundState>>>,
    >,
    /// The closures created by `Promise.prototype.finally`, keyed by function
    /// identity.
    pub(crate) promise_finally: RefCell<
        std::collections::HashMap<u64, Rc<RefCell<crate::builtins::promise::FinallyState>>>,
    >,
    /// The await-resume handlers of running async functions, keyed by function
    /// identity.
    pub(crate) async_resume:
        RefCell<std::collections::HashMap<u64, Rc<crate::async_await::ResumeHandler>>>,
    /// The generator objects' states, keyed by object identity (spec 27.4.3).
    pub(crate) generators:
        RefCell<std::collections::HashMap<u64, Rc<RefCell<crate::generator::GeneratorState>>>>,
    /// The async generator objects' states, keyed by object identity (spec
    /// 27.6.1).
    pub(crate) async_generators: RefCell<
        std::collections::HashMap<u64, Rc<RefCell<crate::async_generator::AsyncGeneratorState>>>,
    >,
    /// The pooled Vms (idle ones; a running Vm is on the native stack and
    /// reached by [`crate::ir::trace_active_vms`]). Their heap-buffered value
    /// stacks are roots the conservative scan cannot see. Boxed so taking and
    /// returning a Vm moves only the pointer: `Vm` is ~1.1KB, and by-value
    /// pool copies dominated the non-leaf call path's profile.
    #[allow(clippy::vec_box)]
    pub(crate) vm_pool: RefCell<Vec<Box<crate::ir::Vm>>>,
    /// Cut 43: per-site compiled bodies, keyed by the shared body `Rc<Block>`
    /// pointer (the canonical site identity `shared_function_body` returns).
    /// Closures from one declaration site share a single `Rc<CompiledBody>`,
    /// so instantiating the site in a loop does not recompile the IR (or, via
    /// the per-body `jit_info` fast pointer, the machine code) per closure.
    /// Traced so the compiled bodies' literal `Value`s stay rooted between
    /// instantiations; entries live for the agent's life.
    pub(crate) compiled_bodies: RefCell<
        std::collections::HashMap<
            usize,
            crate::function::CompiledBodyCacheEntry,
            std::hash::BuildHasherDefault<MixHasher>,
        >,
    >,
    /// Per-source compiled SCRIPT/EVAL bodies, keyed by the exact source text
    /// plus (strict, fast_script, jsx): `run_script`/`perform_eval` re-parse
    /// the same source into a fresh AST each call, so without this an
    /// eval-in-a-loop recompiled the IR per iteration. The key is a heap edge
    /// too, so it is traced here rather than by the map's values-only `Trace`.
    pub(crate) script_bodies:
        RefCell<std::collections::HashMap<ScriptBodyKey, std::rc::Rc<crate::ir::CompiledBody>>>,
    /// Every realm created in this agent (the bootstrap realm plus any the
    /// host creates via `$262.createRealm`), so a realm's builtin called from
    /// another realm can dispatch with its own realm current.
    pub(crate) realms: RefCell<Vec<Handle<Realm>>>,
    /// Memoized owning-realm lookup: function id → the realm whose intrinsic
    /// table holds it (`None` for non-intrinsic functions).
    pub(crate) function_realms: RefCell<std::collections::HashMap<u64, Option<Handle<Realm>>>>,
    /// The [[ShadowRealm]] internal slot of each ShadowRealm instance:
    /// object id → the inner realm it evaluates in.
    pub(crate) shadow_realms: RefCell<IdMap<Handle<Realm>>>,
    /// The wrapped functions the ShadowRealm callable boundary produced:
    /// function id → the target function and the wrapper's [[Realm]].
    pub(crate) shadow_wrapped: RefCell<IdMap<crate::builtins::shadow_realm::ShadowWrapped>>,
    /// The `ShadowRealm.prototype.importValue` continuations, keyed by
    /// function identity.
    pub(crate) shadow_handlers: RefCell<IdMap<crate::builtins::shadow_realm::ShadowHandler>>,
    /// The [[MapData]] of Map instances, keyed by object identity (spec
    /// 24.1.1: a List of entries; `None` marks a deleted ~empty~ slot that
    /// suspended Map iterators skip). The parallel key index makes the
    /// get/has/set/delete probes O(1) on the live entries.
    ///
    /// The entries are `Rc`-shared cells: a Map operation holds the
    /// collection across a user callback (`forEach`, the key's `ToPropertyKey`
    /// conversion), so the entry has to be a cell of its own — and reading it
    /// out of the table must not keep the table borrowed, because another Map
    /// can be created inside that callback.
    pub(crate) map_data: RefCell<IdMap<Rc<RefCell<MapCollection>>>>,
    /// The [[SetData]] of Set instances (spec 24.2.1; `None` is a deleted
    /// ~empty~ slot) plus the element index; see `map_data` for the shape.
    pub(crate) set_data: RefCell<IdMap<Rc<RefCell<SetCollection>>>>,
    /// The [[WeakMapData]] of WeakMap instances, keyed by object identity
    /// (spec 26.3.1). The entry list is a cell for the same reason, and its
    /// keys are traced as ephemerons rather than strong edges.
    pub(crate) weak_map_data: RefCell<IdMap<Rc<RefCell<Vec<MapEntry>>>>>,
    /// The [[WeakSetData]] of WeakSet instances (spec 26.4.1).
    pub(crate) weak_set_data: RefCell<IdMap<Rc<RefCell<Vec<SetEntry>>>>>,
    /// The [[IteratedMap]], [[MapNextIndex]], and [[MapIterationKind]] of Map
    /// iterators, keyed by iterator-object identity (spec 24.1.6). The map
    /// value is `None` once iteration is done.
    pub(crate) map_iter_data: RefCell<IdMap<Rc<RefCell<IterRecord>>>>,
    /// The [[IteratedSet]], [[SetNextIndex]], and [[SetIterationKind]] of Set
    /// iterators (spec 24.2.6).
    pub(crate) set_iter_data: RefCell<IdMap<Rc<RefCell<IterRecord>>>>,
    /// The AsyncFromSyncIterator methods, keyed by function identity.
    pub(crate) async_from_sync: RefCell<
        std::collections::HashMap<u64, std::rc::Rc<crate::async_await::AsyncFromSyncEntry>>,
    >,
    /// The await-resume handlers of async generator bodies, keyed by function
    /// identity.
    pub(crate) async_generator_awaits: RefCell<
        std::collections::HashMap<
            u64,
            std::rc::Rc<crate::async_generator::AsyncGeneratorAwaitEntry>,
        >,
    >,
    /// The iterator-helper objects' states, keyed by object identity (spec
    /// 27.1.3).
    pub(crate) iterator_helpers: RefCell<
        std::collections::HashMap<
            u64,
            std::rc::Rc<RefCell<crate::builtins::iterator::HelperState>>,
        >,
    >,
    /// The wrapped iterators created by `Iterator.from`, keyed by object
    /// identity (spec 27.1.3.2).
    pub(crate) wrapped_iterators: RefCell<
        std::collections::HashMap<
            u64,
            std::rc::Rc<RefCell<crate::builtins::iterator::WrappedIteratorState>>,
        >,
    >,
    /// The async-iterator-helper objects' states, keyed by object identity
    /// (spec 27.1.4).
    pub(crate) async_iterator_helpers: RefCell<
        std::collections::HashMap<
            u64,
            std::rc::Rc<RefCell<crate::builtins::async_iterator::HelperState>>,
        >,
    >,
    /// The await continuations of async-iterator helper drivers, keyed by
    /// function identity.
    pub(crate) async_iterator_awaits: RefCell<
        std::collections::HashMap<u64, std::rc::Rc<crate::builtins::async_iterator::AwaitEntry>>,
    >,
    /// The drivers of eager async-iterator helpers, keyed by driver-object
    /// identity.
    pub(crate) async_iterator_eager: RefCell<
        std::collections::HashMap<
            u64,
            std::rc::Rc<RefCell<crate::builtins::async_iterator::EagerState>>,
        >,
    >,
    /// The capability of the in-flight `next()` of each lazy async-iterator
    /// helper, keyed by helper-object identity.
    pub(crate) async_iterator_pending:
        RefCell<std::collections::HashMap<u64, crate::promise::PromiseCapability>>,
    /// The [[DisposableStackData]] of DisposableStack/AsyncDisposableStack
    /// instances, keyed by object identity (spec 27.4.2).
    pub(crate) disposable_stacks:
        RefCell<IdMap<std::rc::Rc<RefCell<crate::builtins::disposable::DisposableStackData>>>>,
    /// The in-flight `disposeAsync` drivers, keyed by driver-object identity.
    pub(crate) disposable_async_drivers: RefCell<
        std::collections::HashMap<
            u64,
            std::rc::Rc<RefCell<crate::builtins::disposable::AsyncDisposalDriver>>,
        >,
    >,
    /// The capability of each in-flight `disposeAsync`, keyed by driver
    /// identity.
    pub(crate) disposable_async_caps:
        RefCell<std::collections::HashMap<u64, crate::promise::PromiseCapability>>,
    /// The in-flight disposal drivers of async bodies (`using` resources
    /// drained at body completion), keyed by driver-object identity.
    pub(crate) async_body_disposal: RefCell<
        std::collections::HashMap<
            u64,
            std::rc::Rc<RefCell<crate::builtins::disposable::AsyncBodyDisposalDriver>>,
        >,
    >,
    /// The `Array.fromAsync` continuation states, keyed by handler-function
    /// identity (spec 23.1.2.4.1); the bool selects the reject handler.
    pub(crate) array_from_async: RefCell<IdMap<FromAsyncContinuation>>,
    /// The resolve functions of pending `Atomics.waitAsync` waits, keyed by
    /// the registered waiter-event id, with the (block id, byte offset) the
    /// event sits at in the global wait registry. A same-agent `Atomics.notify`
    /// resolves the promise directly with *"ok"* (spec 26.4.15 DoWait step
    /// 20); a cross-thread notify only marks the event, and the owning agent
    /// resolves it from its own thread (`service_wait_async` or the timeout
    /// job, whichever fires first).
    pub(crate) wait_async: RefCell<std::collections::HashMap<u64, (Value, usize, usize)>>,
    /// [[IteratedObject]], [[ArrayIteratorNextIndex]], [[ArrayIterationKind]],
    /// and the iterator box's address (for dead-entry reaping) of Array
    /// iterators, keyed by object identity (spec 23.1.5). A `RefCell` so the
    /// collection's dead-set compaction can drop entries for swept iterators;
    /// otherwise every entry's iterated array stays a root forever.
    pub(crate) array_iter_data:
        RefCell<std::collections::HashMap<u64, (Value, usize, u32, crux::heap::GcAny)>>,
    /// [[GlobalSymbolRegistry]]: `Symbol.for` entries (Phase 8).
    pub(crate) global_symbol_registry: RefCell<Vec<(JsString, Symbol)>>,
    /// The AsyncFromSyncIterator value-unwrap continuations, keyed by
    /// function identity (spec 27.1.5.4).
    pub(crate) async_from_sync_continuations:
        RefCell<IdMap<std::rc::Rc<crate::async_await::AsyncFromSyncContinuationEntry>>>,
    /// The captured V8-style stack trace of Error instances, keyed by object
    /// identity (read by the `%Error.prototype.stack%` accessor; the property
    /// itself is not an own data property, spec 20.5.3.4).
    pub(crate) error_stack: RefCell<IdMap<ErrorStack>>,
    /// The promise the running reaction job's task belongs to — V8's
    /// `current_microtask` as far as an async chain needs it
    /// (`TryGetCurrentTaskPromise`): an await chain is walked from here, so a
    /// stack captured inside a job can report the bodies waiting on it. Set for
    /// the duration of a reaction job and restored after it.
    pub(crate) current_task_promise: Cell<Option<Value>>,
    /// The KeepDuringJob set (spec 26.1.1): WeakRef targets returned by
    /// `deref` in the current job stay alive until the job ends (the
    /// conservative stack scan covers the common case; this covers a deref
    /// result that is discarded immediately). Cleared at each job boundary.
    pub(crate) kept_during_job: RefCell<Vec<Value>>,
    /// GC-4: FinalizationRegistry cleanup jobs enqueued by the collector's
    /// compaction hook (it cannot touch the job queues — the collector runs
    /// with `&self`). Moved into `generic_jobs` by the next job drain; the
    /// job closures' regions are scanned alongside the queued jobs.
    pub(crate) pending_cleanup_jobs: RefCell<Vec<crate::job::Job>>,
    /// [[Cells]] and [[CleanupCallback]] of FinalizationRegistry instances,
    /// keyed by object identity (spec 26.2.1).
    pub(crate) finalization_registries:
        RefCell<IdMap<std::rc::Rc<std::cell::RefCell<crate::builtins::weakref::FinalizationData>>>>,
    /// CreateIterResultObject's cached `{value, done}` shape and the realm it
    /// was forked in (a realm switch refork). The two sequential
    /// `create_data_property` calls it replaces cost ~100-140 ns each on a
    /// fresh object; a generator `next()` spent ~160 ns there. Both handles
    /// are traced below.
    pub(crate) iter_result_realm: std::cell::Cell<Option<Handle<Realm>>>,
    pub(crate) iter_result_map: std::cell::Cell<Option<crux::Handle<crux::Map>>>,
    /// The `%Generator.prototype%` methods `dispatch_call` compares a callee
    /// against, resolved once per prototype generation: every `g.next()`
    /// otherwise paid an intrinsic-table hash, three property lookups and
    /// three string conversions before dispatching. See
    /// [`crate::generator::GeneratorMethods`].
    pub(crate) generator_methods: std::cell::Cell<Option<crate::generator::GeneratorMethods>>,
    /// The for-of fast-verdict cache (Cut 24): "the Array-iteration
    /// infrastructure is stock" — %Array.prototype%'s own @@iterator is the
    /// intrinsic, %ArrayIteratorPrototype% has the stock `next`, and no
    /// `return` on the AIP chain. Re-validated against the three shared
    /// objects' generations.
    pub(crate) for_of_fast_cells: [Cell<Option<ForOfFastVerdict>>; crate::ir::MEMBER_CELLS],
    /// The Array-species fast verdict (this landing): "the species machinery
    /// of this %Array.prototype% is stock" — see [`ArraySpeciesVerdict`].
    /// Direct-mapped on the %Array.prototype% id (different realms' protos
    /// collide on an index only under the id check).
    pub(crate) species_fast_cells: [Cell<Option<ArraySpeciesVerdict>>; crate::ir::MEMBER_CELLS],
    /// The concat-spread verdict (this landing): "the %Array.prototype% ->
    /// %Object.prototype% chain of this %Array.prototype% carries no own
    /// @@isConcatSpreadable" — see [`ConcatSpreadVerdict`]. Direct-mapped on
    /// the %Array.prototype% id.
    pub(crate) spread_cells: [Cell<Option<ConcatSpreadVerdict>>; crate::ir::MEMBER_CELLS],
    /// The fronting array-element value cache (Cut 35 slice 13): (id, index,
    /// generation, value) — a hit returns the element with no
    /// property-vector borrow. Boxed per the Cut 27 lesson.
    pub(crate) array_element_value_cells:
        Box<[Cell<Option<crate::ir::ArrayElementValueCell>>; crate::ir::MEMBER_CELLS]>,
    /// The global call-site leaf cache (Cut 35 slice 12): name → the
    /// resolved leaf entry for a stable global callee, valid while the
    /// global object's identity and generation are unchanged. Boxed so the
    /// Agent's hot-field cache footprint stays small (the Cut 27 lesson).
    pub(crate) global_leaf_cells:
        RefCell<Box<[Option<crate::ir::GlobalLeafCell>; crate::ir::GLOBAL_CELLS]>>,
    /// The slot-callee leaf cache (Cut 35 slice 12): frame-slot index → the
    /// resolved leaf entry for the closure held there, validated by the
    /// callee's heap payload (the held `callee` keeps it alive). Boxed per
    /// the Cut 27 lesson.
    pub(crate) slot_leaf_cells:
        RefCell<Box<[Option<crate::ir::SlotLeafCell>; crate::ir::SLOT_LEAF_CELLS]>>,
    /// Part B, B5.4: cached constructor boilerplate maps — (function id,
    /// prototype id, generation) → the final shape its body's `this.*`
    /// stores transition to, so every construct after the first starts the
    /// object on the final map (see [`crate::ir::ConstructMapCell`]).
    pub(crate) construct_maps:
        Box<[Cell<Option<crate::ir::ConstructMapCell>>; crate::ir::CONSTRUCT_PROTO_CELLS]>,
    /// Cut 73: the pre-forked ordinary-function boilerplate shapes — the
    /// 2-key `length`/`name` map (strict/async/generator/method functions)
    /// and the 4-key map adding the restricted `caller`/`arguments` (sloppy
    /// ordinary functions), forked ONCE per agent and reused by every
    /// `register_function` (the (key, attrs) inputs are process constants, so
    /// the cache never invalidates). Indexed by `restricted`; traced below so
    /// a collection between creates cannot free the shape (`function.rs`
    /// `function_boilerplate_map`).
    pub(crate) function_boilerplate_maps: [Cell<Option<crux::Handle<crux::Map>>>; 2],
    /// The for-in enumeration cache (this landing): base id -> the
    /// enumerated (level, key) list + the full chain snapshot for a base
    /// whose whole prototype chain is generation-tracked (see
    /// [`crate::ir::ForInEnumCache`]). Direct-mapped on the base id and
    /// traced (each entry retains its base — and through it the chain —
    /// plus the key Values). A ForInBegin probe re-walks the LIVE chain
    /// comparing (id, generation); an unchanged chain reuses the cached
    /// list instead of re-enumerating (own_property_keys + per-key
    /// descriptor reads + key boxing) on every re-entry. Boxed per the Cut
    /// 27 lesson.
    pub(crate) for_in_cells:
        RefCell<Box<[Option<crate::ir::ForInEnumCache>; crate::ir::FOR_IN_CELLS]>>,
    /// The leaf-inline cache (Cut 34): function id → the record data
    /// `do_call_fast` needs (compiled ir, strictness, closure env), so a hot
    /// leaf call skips the `ecma_functions` HashMap lookup. Boxed per the
    /// Cut 27 lesson.
    pub(crate) leaf_cache: RefCell<Box<[LeafCacheCell; crate::ir::LEAF_CACHE]>>,
    /// The IC value cells, in the roots like the rest of the traced state.
    /// They are `Cell`s (not `RefCell`s): the payloads are `Copy`, so a read
    /// is a load and a write a store, with no borrow flag on the hot path.
    /// `Cell` is `#[repr(transparent)]`, which is also what keeps the compiled
    /// lanes' addressing intact — see [`AgentRoots::member_value_cell_base`].
    ///
    /// `member_value_cells` is the fronting read-side cache (Cut 35 slice 11):
    /// (id, name, generation, value), a plain `#[repr(C)]` array so the
    /// compiled `GetMemberName` probe reads the cells at fixed offsets;
    /// `MemberValueCell::empty()`'s impossible id never validates.
    pub(crate) member_value_cells: [Cell<crate::ir::MemberValueCell>; crate::ir::MEMBER_CELLS],
    /// The computed-read value cache (G8): (key Value, interned atom) for
    /// `o[k]`, keyed by the key's identity rather than an interned atom so the
    /// compiled probe can run before the key is converted; traced, because the
    /// key box must outlive the cell, which is what makes the identity match
    /// sound.
    pub(crate) computed_read_cells:
        [Cell<crate::ir::ComputedReadCell>; crate::ir::COMPUTED_READ_CELLS],
    /// The JIT's direct-mapped global-VALUE cells (Cut 36): `name` + the
    /// capturing `(global_id, generation)` validate the cached value against
    /// the global object's LIVE identity and generation (the generation bumps
    /// on any own-property change), so the compiled `LoadGlobal` fast path
    /// reads the value in place and only calls `get_global` on a miss.
    /// `load_global_value` repopulates the cell on every successful read.
    pub(crate) global_value_cells: [Cell<crate::jit::GlobalValueCell>; crate::ir::GLOBAL_CELLS],
    /// The frames of each host stack-trace capture (v8::StackTrace), keyed by
    /// the *box address* of the object the capture handed back — that object
    /// exists so the entry has a liveness, and [`AgentRoots::compact_weak_tables`]
    /// drops entries whose object is dead. A leak-free version of what
    /// `error_stack` does with an identity key, and the reason the key is an
    /// address rather than an id.
    pub(crate) stack_traces: RefCell<std::collections::HashMap<usize, Vec<crate::api::StackFrame>>>,
    /// [[WeakRefTarget]] of WeakRef instances, keyed by object identity
    /// (spec 26.1.1: the target is held weakly — `deref` returns it while it
    /// is reachable, `undefined` once a collection clears it; GC-4).
    pub(crate) weak_ref_targets: RefCell<std::collections::HashMap<u64, Value>>,
    /// The box address of every promise, so a sweep can drop the entry of the
    /// promises it reclaimed ([`AgentRoots::reap_promise_addrs`]).
    pub(crate) promise_addrs: RefCell<std::collections::HashMap<u64, crux::heap::GcAny>>,
    /// Promise ids whose address entry a sweep dropped, drained by
    /// `Agent::reap_dead_promises` at the next run entry.
    pending_dead_promises: RefCell<Vec<u64>>,
    /// The wasm JS-API wrapper tables (Cut 10). Their values are heap edges,
    /// so they are roots: an untraced `Value` here is freed, its object id
    /// reused, and a later lookup returns the wrong object (e.g.
    /// `Instance.exports` yielding a Promise). The index-only wasm tables
    /// (`wasm_exports`, `wasm_memories`, `wasm_tables`, `wasm_globals`,
    /// `wasm_tags`, and the engine store) stay on the agent: they hold object
    /// ids and store cells, which the collector never reads.
    #[cfg(feature = "wasm")]
    pub(crate) wasm_streaming_handlers: RefCell<
        std::collections::HashMap<u64, std::rc::Rc<crate::builtins::wasm::StreamingHandler>>,
    >,
    #[cfg(feature = "wasm")]
    pub(crate) wasm_instance_exports: RefCell<std::collections::HashMap<u64, Value>>,
    #[cfg(feature = "wasm")]
    pub(crate) wasm_func_objects: RefCell<std::collections::HashMap<wasm::FuncKey, Value>>,
    #[cfg(feature = "wasm")]
    pub(crate) wasm_memory_objects: RefCell<std::collections::HashMap<usize, Value>>,
    #[cfg(feature = "wasm")]
    pub(crate) wasm_memory_buffers: RefCell<std::collections::HashMap<usize, Value>>,
    #[cfg(feature = "wasm")]
    pub(crate) wasm_table_objects: RefCell<std::collections::HashMap<usize, Value>>,
    #[cfg(feature = "wasm")]
    pub(crate) wasm_global_objects: RefCell<std::collections::HashMap<usize, Value>>,
    #[cfg(feature = "wasm")]
    pub(crate) wasm_host_functions:
        RefCell<std::collections::HashMap<u64, (Value, wasm::types::FuncType)>>,
    #[cfg(feature = "wasm")]
    pub(crate) wasm_tag_objects: RefCell<std::collections::HashMap<usize, Value>>,
    #[cfg(feature = "wasm")]
    pub(crate) wasm_js_exceptions: RefCell<std::collections::HashMap<usize, Value>>,
    #[cfg(feature = "wasm")]
    pub(crate) wasm_extern_values: RefCell<std::collections::HashMap<u32, Value>>,
    /// The internal state of ArrayBuffer/SharedArrayBuffer objects, keyed by
    /// object identity (spec 25.1.1: [[ArrayBufferData]],
    /// [[ArrayBufferByteLength]], [[ArrayBufferMaxByteLength]], and the
    /// resizable/growable + shared flags).
    ///
    /// The entry is not a cell of its own: every reader takes a `Ref` into the
    /// table and drops it before anything that can allocate (a coercion, the
    /// species constructor), which is the discipline the buffer builtins
    /// already followed for the per-entry cell. Inserting while one is held
    /// therefore *panics*, so a new buffer must be allocated before the state
    /// of an existing one is read — never in the middle of the read.
    pub(crate) buffer_data: RefCell<IdMap<crate::builtins::array_buffer::BufferState>>,
    /// The [[ViewedArrayBuffer]], [[ByteLength]], and [[ByteOffset]] of
    /// DataView instances, keyed by object identity (spec 25.4.2).
    pub(crate) dataview_data: RefCell<IdMap<crate::builtins::dataview::DataViewState>>,
    /// Host-provided module sources, keyed by specifier (HostResolveImportedModule).
    pub(crate) host_modules:
        RefCell<std::collections::HashMap<crux::string::JsString, crate::module::HostModuleSource>>,
    /// The module behind each namespace object, keyed by object identity.
    pub(crate) module_namespaces:
        RefCell<std::collections::HashMap<u64, Handle<crate::module::SourceTextModule>>>,
    /// The module behind each deferred namespace object ([[Deferred]] =
    /// true), keyed by object identity.
    pub(crate) deferred_namespaces:
        RefCell<std::collections::HashMap<u64, Handle<crate::module::SourceTextModule>>>,
    /// The module behind each `%AbstractModuleSource%` instance, keyed by
    /// object identity.
    pub(crate) module_sources:
        RefCell<std::collections::HashMap<u64, Handle<crate::module::SourceTextModule>>>,
    /// The dynamic-import namespace resolvers, keyed by function identity.
    pub(crate) import_namespace_resolvers: RefCell<std::collections::HashMap<u64, (Value, Value)>>,
    /// The wait state of an `import.defer(...).then(...)` promise: (remaining,
    /// capability resolve, capability reject, module).
    pub(crate) deferred_module_waits:
        RefCell<std::collections::HashMap<u64, crate::module::DeferredWait>>,
    /// The module behind each DeferredModule `.then` method (import-defer),
    /// keyed by function identity.
    pub(crate) deferred_module_thens:
        RefCell<std::collections::HashMap<u64, Handle<crate::module::SourceTextModule>>>,
    /// The in-flight module evaluations (spec 16.2.2.5 InnerModuleEvaluation
    /// stack): used to detect dependency cycles and find the cycle root.
    pub(crate) module_eval_stack:
        RefCell<Vec<crux::handle::Handle<crate::module::SourceTextModule>>>,
    /// Cut 43: per-site compiled bodies, keyed by the shared body `Rc<Block>`
    /// pointer (the canonical site identity `shared_function_body` returns) —
    /// now in [`AgentRoots`], with the rationale.
    /// The realm a job is running in, set by the drain for the job's own realm
    /// and nothing else (spec 9.5.4 hands the host a realm to run a job in).
    ///
    /// It is V8's `isolate->context()`: [`current_realm`](Self::current_realm)
    /// prefers a running execution context and falls back to this, which before
    /// was an error — a job runs with no execution context of its own, so the
    /// realm carried by the queue is the only one an engine hook fired from a
    /// job can answer.
    pub(crate) entered_realm: Cell<Option<Handle<Realm>>>,
    /// The [[InitializedCollator]] records of Intl.Collator instances,
    /// keyed by object identity (ECMA-402 §10).
    pub(crate) intl_collator_data: RefCell<IdMap<crate::builtins::intl::collator::CollatorRecord>>,
    /// The [[InitializedDateTimeFormat]] records of Intl.DateTimeFormat
    /// instances, keyed by object identity (ECMA-402 §11).
    pub(crate) intl_date_time_format_data:
        RefCell<IdMap<crate::builtins::intl::date_time_format::DateTimeFormatRecord>>,
    /// The [[InitializedNumberFormat]] records of Intl.NumberFormat
    /// instances, keyed by object identity (ECMA-402 §16: the resolved
    /// options).
    pub(crate) intl_number_format_data:
        RefCell<IdMap<crate::builtins::intl::number_format::NumberFormatRecord>>,
    /// The [[InitializedPluralRules]] records of Intl.PluralRules
    /// instances, keyed by object identity (ECMA-402 §17: the plural type
    /// and the digit/notation options).
    pub(crate) intl_plural_rules_data:
        RefCell<IdMap<crate::builtins::intl::plural_rules::PluralRulesRecord>>,
    /// The [[SegmentsSegmenter]]/[[SegmentsString]] slots of Intl.Segmenter
    /// `segment()` results (ECMA-402 §19.5).
    pub(crate) intl_segments_data: RefCell<IdMap<crate::builtins::intl::segmenter::SegmentsRecord>>,
    /// The [[IteratingSegmenter]]/[[IteratedString]]/
    /// [[IteratedStringNextSegmentCodeUnitIndex]] slots of segment
    /// iterators (ECMA-402 §19.6).
    pub(crate) intl_segment_iterator_data:
        RefCell<IdMap<crate::builtins::intl::segmenter::SegmentIteratorRecord>>,
    /// The [[InitializedTemporal*]] records of Temporal instances, keyed by
    /// object identity (the proposal-temporal internal slots).
    pub(crate) temporal_data: RefCell<IdMap<crate::builtins::temporal::TemporalRecord>>,
    /// The [[Calendar]] internal slot of Temporal instances, keyed by object
    /// identity (the proposal-temporal calendar field; default "iso8601").
    pub(crate) temporal_calendars: RefCell<IdMap<crux::string::JsString>>,
    /// [[IteratedString]] and [[StringIteratorNextIndex]] of String iterators,
    /// keyed by object identity (spec 22.1.5: the iterator's internal slots).
    pub(crate) string_iter_data: RefCell<IdMap<(Option<crux::string::JsString>, u64)>>,
    /// [[IteratingRegExp]], [[IteratedString]], [[Global]], [[Unicode]], and
    /// [[Done]] of RegExp String iterators (spec 22.2.6).
    pub(crate) regexp_string_iter_data: RefCell<IdMap<RegexpStringIterRecord>>,
    /// [[SymbolData]] of Symbol wrapper objects, keyed by object identity
    /// (spec 20.4.1: `Object(sym)` boxes the symbol).
    pub(crate) symbol_data: RefCell<IdMap<crux::symbol::Symbol>>,
    /// [[BigIntData]] of BigInt wrapper objects, keyed by object identity
    /// (spec 21.2.1: `Object(5n)` boxes the BigInt).
    pub(crate) bigint_data: RefCell<IdMap<crux::BigInt>>,
    /// The RegExp internal state ([[OriginalSource]], [[OriginalFlags]],
    /// [[RegExpRecord]], [[RegExpMatcher]]) of RegExp instances, keyed by
    /// object identity (spec 22.2.5). The state is handed out as a clone: its
    /// compiled program is shared through an `Rc`, so the copy is one refcount
    /// bump, and no reader can hold a guard across an exec.
    pub(crate) regexp_data: RefCell<IdMap<crate::builtins::regexp::RegExpState>>,
    /// The [[RawJSON]] text of JSON.rawJSON objects, keyed by object identity
    /// (spec 26.6.3).
    pub(crate) raw_json_data: RefCell<IdMap<JsString>>,
    /// The ECMAScript-function records, keyed by function identity: the spec
    /// 10.2.1 slots [[Environment]], [[FormalParameters]], [[ECMAScriptCode]],
    /// [[ThisMode]], [[HomeObject]].
    ///
    /// A record is read on every call and written when a class, a method's
    /// home object or a lazy `prototype` is set up, so both sides reach it
    /// through [`Agent::ecma_function`]/[`Agent::ecma_function_mut`], which
    /// hand out a guard. Hold one only while fields are copied out: a guard
    /// held across an allocation aborts that collection, and one held across
    /// an insert into the same table panics.
    pub(crate) ecma_functions: RefCell<IdMap<crate::function::EcmaFunction>>,
}

impl AgentRoots {
    /// The `JitCallContext` base of the member value cells.
    ///
    /// The compiled lanes index it as `*mut MemberValueCell` at
    /// `index * size_of::<MemberValueCell>()`; `Cell<T>` is
    /// `#[repr(transparent)]` over `T`, so the `Cell` array's element-0 address
    /// and stride are the payload's, and the box's allocation is fixed for the
    /// agent's life, so the address never moves.
    pub(crate) fn member_value_cell_base(&self) -> *mut std::os::raw::c_void {
        self.member_value_cells
            .as_slice()
            .as_ptr()
            .cast_mut()
            .cast()
    }

    /// The `JitCallContext` base of the computed-read cells (see
    /// [`AgentRoots::member_value_cell_base`]).
    pub(crate) fn computed_read_cell_base(&self) -> *mut std::os::raw::c_void {
        self.computed_read_cells
            .as_slice()
            .as_ptr()
            .cast_mut()
            .cast()
    }

    /// The `JitCallContext` base of the global value cells (see
    /// [`AgentRoots::member_value_cell_base`]).
    pub(crate) fn global_value_cell_base(&self) -> *mut std::os::raw::c_void {
        self.global_value_cells
            .as_slice()
            .as_ptr()
            .cast_mut()
            .cast()
    }

    /// GC-2: the thread-scoped roots of a collection — the Vms currently
    /// running bodies (their heap-buffered value stacks are invisible to the
    /// conservative stack scan) and the opaque job closures' captured regions.
    /// Nothing here is addressed through an `Agent`: the containers are the
    /// roots' own and the rest belongs to the thread.
    pub(crate) fn trace_active_run_state(&self, visit: &mut dyn FnMut(GcAny)) {
        // GC-2: the Vms currently running bodies (their heap-buffered value
        // stacks are invisible to the conservative stack scan). Pooled and
        // suspended Vms are covered by `vm_pool` and the generator/async
        // tables above.
        crate::ir::trace_active_vms(visit);
        // GC-2: the opaque job closures hold captured Values; scan their
        // boxes conservatively (queued jobs plus the job currently running).
        let mut regions: Vec<(*const u8, usize)> = Vec::new();
        if !self.collect_job_regions(&mut regions) {
            crux::heap::note_aborted_trace();
            return;
        }
        RUNNING_JOB_REGION.with(|region| {
            if let Some(region) = &*region.borrow() {
                regions.push(*region);
            }
        });
        if !regions.is_empty() {
            crux::heap::scan_regions(&regions, visit);
        }
    }

    /// The realm a collection-time reaction (a FinalizationRegistry cleanup
    /// job) belongs to: the running execution context's, else the realm the
    /// last job was entered for. `None` before any context has run.
    fn collection_realm(&self) -> Option<Handle<Realm>> {
        if let Some(context) = self.execution_context_stack.last() {
            return Some(context.realm);
        }
        self.entered_realm.get()
    }

    /// GC-4: whether any weak structure exists — the collector then runs a
    /// scan-free mark so the weak tables compact against true heap
    /// reachability instead of stale stack words. A `RefCell` mid-borrow
    /// (the tables are read-only here) defaults to `true`: the precise pass
    /// is harmless when the tables are empty.
    fn has_weak_structures(&self) -> bool {
        !self.weak_map_data.borrow().is_empty()
            || !self.weak_set_data.borrow().is_empty()
            || !self.finalization_registries.borrow().is_empty()
            || self
                .stack_traces
                .try_borrow()
                .is_ok_and(|traces| !traces.is_empty())
            || self
                .weak_ref_targets
                .try_borrow()
                .is_ok_and(|targets| !targets.is_empty())
    }

    /// GC-3/GC-4: the pre-sweep compaction — drop the WeakMap/WeakSet
    /// entries and WeakRef targets whose key (or element, or target) box is
    /// dead, and process the FinalizationRegistry cells: a dead target's
    /// cell is removed and its held value captured into a cleanup job
    /// (retained so the sweep does not free it); a live cell's dead
    /// unregister token is cleared. Runs while the dead boxes are still
    /// allocated, so the entry values are readable.
    fn compact_weak_tables(&self, dead: &[usize], retain: &mut dyn FnMut(crux::heap::GcAny)) {
        if dead.is_empty() {
            return;
        }
        // The collector sorts the dead set before the hook runs, so a
        // membership test is a binary search — the SipHash HashSet of every
        // dead address was measurable per collection.
        let is_dead = |addr: usize| dead.binary_search(&addr).is_ok();
        // A stack-trace capture is held under the box address of the object it
        // hands back, so a capture the host stopped holding is dropped here —
        // the hook runs between the mark and the sweep, while the dead boxes are
        // still allocated and their addresses cannot yet have been reused.
        self.stack_traces
            .borrow_mut()
            .retain(|address, _| !is_dead(*address));
        for cell in self.weak_map_data.borrow().values() {
            let mut data = cell.borrow_mut();
            data.retain(|entry| match entry {
                Some((key, _)) => match value_box_addr(key) {
                    Some(key) => !is_dead(key.addr()),
                    // A non-heap key cannot have been swept.
                    None => true,
                },
                None => false,
            });
        }
        for cell in self.weak_set_data.borrow().values() {
            let mut data = cell.borrow_mut();
            data.retain(|entry| match entry {
                Some(element) => match value_box_addr(element) {
                    Some(element) => !is_dead(element.addr()),
                    None => true,
                },
                None => false,
            });
        }
        // GC-4 WeakRef: a dead target clears the entry (deref → undefined).
        for (_, target) in self.weak_ref_targets.borrow_mut().iter_mut() {
            if let Some(target_box) = value_box_addr(target)
                && is_dead(target_box.addr())
            {
                *target = Value::Undefined;
            }
        }
        // GC-4 FinalizationRegistry: dead-target cells are removed and
        // their held values captured into a cleanup job (retained so the
        // sweep keeps them); dead unregister tokens on live cells clear.
        let Some(realm) = self.collection_realm() else {
            return;
        };
        for data in self.finalization_registries.borrow().values() {
            let mut data = data.borrow_mut();
            let mut cells = std::mem::take(&mut data.cells);
            let mut i = 0;
            while i < cells.len() {
                let cell = &cells[i];
                let target_dead =
                    value_box_addr(&cell.target).is_some_and(|target| is_dead(target.addr()));
                if target_dead {
                    let cell = cells.swap_remove(i);
                    let callback = data.callback;
                    let held_value = cell.held_value;
                    if let Some(held) = value_box_addr(&held_value) {
                        retain(held);
                    }
                    let cleanup_closure = move |agent: &mut crate::agent::Agent| {
                        crate::function::call(agent, &callback, Value::Undefined, &[held_value])
                    };
                    let job = crate::job::Job::new(Some(realm), cleanup_closure);
                    self.pending_cleanup_jobs.borrow_mut().push(job);
                } else {
                    // Live cell: clear a dead unregister token so the
                    // dangling handle is never compared.
                    if let Some(token) = &mut cells[i].unregister_token
                        && let Some(token_box) = value_box_addr(token)
                        && is_dead(token_box.addr())
                    {
                        cells[i].unregister_token = None;
                    }
                    i += 1;
                }
            }
            data.cells = cells;
        }
    }

    /// Drop the `array_iter_data` entries of Array iterators the sweep
    /// reclaimed. `dead` is the sweep's address list (the conservative mark's
    /// result, while no allocation can have reused a slot). Without this, every
    /// dead iterator's entry keeps its iterated array a root forever, and the
    /// map's trace grows with the number of iterators ever created.
    fn reap_dead_iterators(&self, dead: &[usize]) {
        if dead.is_empty() {
            return;
        }
        let Ok(mut map) = self.array_iter_data.try_borrow_mut() else {
            return;
        };
        if map.is_empty() {
            return;
        }
        map.retain(|_, (_, _, _, iterator)| dead.binary_search(&iterator.addr()).is_err());
    }

    fn reap_promise_addrs(&self, dead: &[usize]) {
        if dead.is_empty() {
            return;
        }
        let Ok(mut pending) = self.pending_dead_promises.try_borrow_mut() else {
            return;
        };
        let Ok(mut addrs) = self.promise_addrs.try_borrow_mut() else {
            return;
        };
        addrs.retain(|id, addr| {
            if dead.binary_search(&addr.addr()).is_ok() {
                pending.push(*id);
                false
            } else {
                true
            }
        });
    }

    /// Drop the states of generators whose object the sweep reclaimed. Without
    /// this every generator ever created leaves a rooted state in the table —
    /// its environment, execution context, suspended VM and arguments stay
    /// reachable — so the traced root set grows with the number of generators
    /// the program has created and every collection scans them. The sweep's
    /// address list is the conservative mark's result, so a generator object
    /// held only in a Rust stack local is never pruned.
    fn reap_dead_generators(&self, dead: &[usize]) {
        if dead.is_empty() {
            return;
        }
        let Ok(mut map) = self.generators.try_borrow_mut() else {
            return;
        };
        if map.is_empty() {
            return;
        }
        map.retain(|_, state| match state.try_borrow() {
            Ok(state) => dead.binary_search(&state.object_addr).is_err(),
            // A state borrowed here is a generator body executing across the
            // collection: alive by definition.
            Err(_) => true,
        });
        drop(map);
        let Ok(mut map) = self.async_generators.try_borrow_mut() else {
            return;
        };
        map.retain(|_, state| match state.try_borrow() {
            Ok(state) => dead.binary_search(&state.object_addr).is_err(),
            Err(_) => true,
        });
    }

    fn new() -> AgentRoots {
        AgentRoots {
            live_registry: std::cell::Cell::new(std::ptr::null()),
            execution_context_stack: ContextStack::new(),
            promise_jobs: RefCell::new(VecDeque::new()),
            generic_jobs: RefCell::new(VecDeque::new()),
            timeout_jobs: RefCell::new(VecDeque::new()),
            microtask_queues: RefCell::new(Vec::new()),
            kept_alive: RefCell::new(Vec::new()),
            promises: RefCell::new(std::collections::HashMap::new()),
            promise_resolvers: RefCell::new(std::collections::HashMap::new()),
            promise_compound: RefCell::new(std::collections::HashMap::new()),
            promise_finally: RefCell::new(std::collections::HashMap::new()),
            async_resume: RefCell::new(std::collections::HashMap::new()),
            generators: RefCell::new(std::collections::HashMap::new()),
            async_generators: RefCell::new(std::collections::HashMap::new()),
            vm_pool: RefCell::new(Vec::new()),
            compiled_bodies: RefCell::new(std::collections::HashMap::default()),
            script_bodies: RefCell::new(std::collections::HashMap::new()),
            realms: RefCell::new(Vec::new()),
            function_realms: RefCell::new(std::collections::HashMap::new()),
            shadow_realms: RefCell::new(std::collections::HashMap::default()),
            shadow_wrapped: RefCell::new(std::collections::HashMap::default()),
            shadow_handlers: RefCell::new(std::collections::HashMap::default()),
            ecma_functions: RefCell::new(IdMap::default()),
            map_data: RefCell::new(IdMap::default()),
            set_data: RefCell::new(IdMap::default()),
            weak_map_data: RefCell::new(IdMap::default()),
            weak_set_data: RefCell::new(IdMap::default()),
            map_iter_data: RefCell::new(IdMap::default()),
            set_iter_data: RefCell::new(IdMap::default()),
            async_from_sync: RefCell::new(std::collections::HashMap::new()),
            async_generator_awaits: RefCell::new(std::collections::HashMap::new()),
            iterator_helpers: RefCell::new(std::collections::HashMap::new()),
            wrapped_iterators: RefCell::new(std::collections::HashMap::new()),
            async_iterator_helpers: RefCell::new(std::collections::HashMap::new()),
            async_iterator_awaits: RefCell::new(std::collections::HashMap::new()),
            async_iterator_eager: RefCell::new(std::collections::HashMap::new()),
            async_iterator_pending: RefCell::new(std::collections::HashMap::new()),
            async_from_sync_continuations: RefCell::new(IdMap::default()),
            error_stack: RefCell::new(IdMap::default()),
            global_symbol_registry: RefCell::new(Vec::new()),
            current_task_promise: Cell::new(None),
            kept_during_job: RefCell::new(Vec::new()),
            pending_cleanup_jobs: RefCell::new(Vec::new()),
            finalization_registries: RefCell::new(IdMap::default()),
            iter_result_realm: Cell::new(None),
            iter_result_map: Cell::new(None),
            generator_methods: Cell::new(None),
            for_of_fast_cells: std::array::from_fn(|_| Cell::new(None)),
            species_fast_cells: std::array::from_fn(|_| Cell::new(None)),
            spread_cells: std::array::from_fn(|_| Cell::new(None)),
            for_in_cells: RefCell::new(Box::new(std::array::from_fn(|_| None))),
            array_element_value_cells: Box::new(std::array::from_fn(|_| Cell::new(None))),
            global_leaf_cells: RefCell::new(Box::new(std::array::from_fn(|_| None))),
            slot_leaf_cells: RefCell::new(Box::new(std::array::from_fn(|_| None))),
            leaf_cache: RefCell::new(Box::new(std::array::from_fn(|_| None))),
            member_value_cells: std::array::from_fn(|_| {
                Cell::new(crate::ir::MemberValueCell::empty())
            }),
            computed_read_cells: std::array::from_fn(|_| {
                Cell::new(crate::ir::ComputedReadCell::empty())
            }),
            global_value_cells: std::array::from_fn(|_| {
                Cell::new(crate::jit::GlobalValueCell::empty())
            }),
            stack_traces: RefCell::new(std::collections::HashMap::new()),
            weak_ref_targets: RefCell::new(std::collections::HashMap::new()),
            promise_addrs: RefCell::new(std::collections::HashMap::new()),
            pending_dead_promises: RefCell::new(Vec::new()),
            #[cfg(feature = "wasm")]
            wasm_streaming_handlers: RefCell::new(std::collections::HashMap::new()),
            #[cfg(feature = "wasm")]
            wasm_instance_exports: RefCell::new(std::collections::HashMap::new()),
            #[cfg(feature = "wasm")]
            wasm_func_objects: RefCell::new(std::collections::HashMap::new()),
            #[cfg(feature = "wasm")]
            wasm_memory_objects: RefCell::new(std::collections::HashMap::new()),
            #[cfg(feature = "wasm")]
            wasm_memory_buffers: RefCell::new(std::collections::HashMap::new()),
            #[cfg(feature = "wasm")]
            wasm_table_objects: RefCell::new(std::collections::HashMap::new()),
            #[cfg(feature = "wasm")]
            wasm_global_objects: RefCell::new(std::collections::HashMap::new()),
            #[cfg(feature = "wasm")]
            wasm_host_functions: RefCell::new(std::collections::HashMap::new()),
            #[cfg(feature = "wasm")]
            wasm_tag_objects: RefCell::new(std::collections::HashMap::new()),
            #[cfg(feature = "wasm")]
            wasm_js_exceptions: RefCell::new(std::collections::HashMap::new()),
            #[cfg(feature = "wasm")]
            wasm_extern_values: RefCell::new(std::collections::HashMap::new()),
            construct_maps: Box::new(std::array::from_fn(|_| Cell::new(None))),
            function_boilerplate_maps: std::array::from_fn(|_| Cell::new(None)),
            intl_number_format_data: RefCell::new(IdMap::default()),
            intl_plural_rules_data: RefCell::new(IdMap::default()),
            intl_date_time_format_data: RefCell::new(IdMap::default()),
            intl_collator_data: RefCell::new(IdMap::default()),
            intl_segments_data: RefCell::new(IdMap::default()),
            intl_segment_iterator_data: RefCell::new(IdMap::default()),
            temporal_data: RefCell::new(IdMap::default()),
            temporal_calendars: RefCell::new(IdMap::default()),
            string_iter_data: RefCell::new(IdMap::default()),
            regexp_string_iter_data: RefCell::new(IdMap::default()),
            host_modules: RefCell::new(std::collections::HashMap::new()),
            module_namespaces: RefCell::new(std::collections::HashMap::new()),
            deferred_namespaces: RefCell::new(std::collections::HashMap::new()),
            module_sources: RefCell::new(std::collections::HashMap::new()),
            import_namespace_resolvers: RefCell::new(std::collections::HashMap::new()),
            deferred_module_waits: RefCell::new(std::collections::HashMap::new()),
            deferred_module_thens: RefCell::new(std::collections::HashMap::new()),
            module_eval_stack: RefCell::new(Vec::new()),
            entered_realm: Cell::new(None),
            disposable_stacks: RefCell::new(IdMap::default()),
            disposable_async_drivers: RefCell::new(std::collections::HashMap::new()),
            disposable_async_caps: RefCell::new(std::collections::HashMap::new()),
            async_body_disposal: RefCell::new(std::collections::HashMap::new()),
            array_from_async: RefCell::new(IdMap::default()),
            wait_async: RefCell::new(std::collections::HashMap::new()),
            array_iter_data: RefCell::new(std::collections::HashMap::new()),
            buffer_data: RefCell::new(IdMap::default()),
            dataview_data: RefCell::new(IdMap::default()),
            symbol_data: RefCell::new(IdMap::default()),
            bigint_data: RefCell::new(IdMap::default()),
            regexp_data: RefCell::new(IdMap::default()),
            raw_json_data: RefCell::new(IdMap::default()),
        }
    }

    /// GC-2: the closure regions of every queued job. The opaque closures hold
    /// captured `Value`s the conservative stack scan cannot see, so the caller
    /// scans them. `false` means a queue was borrowed mid-collection: the
    /// caller aborts the sweep instead of tracing an incomplete root set.
    fn collect_job_regions(&self, regions: &mut Vec<(*const u8, usize)>) -> bool {
        let Ok(promise_jobs) = self.promise_jobs.try_borrow() else {
            return false;
        };
        let Ok(generic_jobs) = self.generic_jobs.try_borrow() else {
            return false;
        };
        let Ok(timeout_jobs) = self.timeout_jobs.try_borrow() else {
            return false;
        };
        let Ok(queues) = self.microtask_queues.try_borrow() else {
            return false;
        };
        for job in promise_jobs.iter().chain(generic_jobs.iter()) {
            regions.push(job.closure_region());
        }
        for (_, job) in timeout_jobs.iter() {
            regions.push(job.closure_region());
        }
        // GC-4: the pending FinalizationRegistry cleanup jobs (captured held
        // values and callbacks) ride alongside the queued jobs.
        let Ok(cleanup) = self.pending_cleanup_jobs.try_borrow() else {
            return false;
        };
        for job in cleanup.iter() {
            regions.push(job.closure_region());
        }
        // A host's own queue holds the same opaque closures, and its jobs are
        // as invisible to the stack scan as the default queue's — a handler
        // queued here and waited on across an eval was swept without this, and
        // the job then failed with "Function body is not registered".
        for queue in queues.iter().flatten() {
            for job in &queue.jobs {
                regions.push(job.closure_region());
            }
        }
        true
    }
}

impl Trace for AgentRoots {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.execution_context_stack.trace(visit);
        self.promise_jobs.trace(visit);
        if let Ok(queues) = self.microtask_queues.try_borrow() {
            for queue in queues.iter().flatten() {
                queue.jobs.trace(visit);
            }
        } else {
            crux::heap::note_aborted_trace();
        }
        self.generic_jobs.trace(visit);
        if let Ok(jobs) = self.timeout_jobs.try_borrow() {
            for (_, job) in jobs.iter() {
                job.trace(visit);
            }
        } else {
            crux::heap::note_aborted_trace();
        }
        self.kept_alive.trace(visit);
        self.promises.trace(visit);
        self.promise_resolvers.trace(visit);
        self.promise_compound.trace(visit);
        self.promise_finally.trace(visit);
        self.async_resume.trace(visit);
        self.generators.trace(visit);
        self.async_generators.trace(visit);
        if let Ok(pool) = self.vm_pool.try_borrow() {
            for vm in pool.iter() {
                vm.trace(visit);
            }
        } else {
            crux::heap::note_aborted_trace();
        }
        if let Ok(bodies) = self.compiled_bodies.try_borrow() {
            for entry in bodies.values() {
                entry.compiled.trace(visit);
            }
        } else {
            crux::heap::note_aborted_trace();
        }
        if let Ok(bodies) = self.script_bodies.try_borrow() {
            for (key, body) in bodies.iter() {
                key.0.trace(visit);
                body.trace(visit);
            }
        } else {
            crux::heap::note_aborted_trace();
        }
        self.realms.trace(visit);
        self.function_realms.trace(visit);
        self.shadow_realms.trace(visit);
        self.shadow_wrapped.trace(visit);
        self.shadow_handlers.trace(visit);
        self.ecma_functions.trace(visit);
        self.dataview_data.trace(visit);
        for cell in self.array_element_value_cells.iter() {
            if let Some(cell) = cell.get() {
                cell.trace(visit);
            }
        }
        for cell in self.for_of_fast_cells.iter() {
            if let Some(cell) = cell.get() {
                cell.trace(visit);
            }
        }
        for cell in self.species_fast_cells.iter() {
            if let Some(cell) = cell.get() {
                cell.trace(visit);
            }
        }
        for cell in self.spread_cells.iter() {
            if let Some(cell) = cell.get() {
                cell.trace(visit);
            }
        }
        for cell in self.for_in_cells.borrow().iter().flatten() {
            cell.trace(visit);
        }
        for cell in self.global_leaf_cells.borrow().iter().flatten() {
            cell.trace(visit);
        }
        for cell in self.slot_leaf_cells.borrow().iter().flatten() {
            cell.trace(visit);
        }
        for (_, entry) in self.leaf_cache.borrow().iter().flatten() {
            entry.trace(visit);
        }
        // The IC value caches hold Values; the index-only caches re-resolve
        // from the (traced) objects and need no tracing.
        for cell in self.member_value_cells.iter() {
            cell.get().trace(visit);
        }
        for cell in self.computed_read_cells.iter() {
            cell.get().trace(visit);
        }
        for cell in self.global_value_cells.iter() {
            cell.get().trace(visit);
        }
        #[cfg(feature = "wasm")]
        {
            self.wasm_streaming_handlers.borrow().trace(visit);
            for value in self.wasm_instance_exports.borrow().values() {
                value.trace(visit);
            }
            for value in self.wasm_func_objects.borrow().values() {
                value.trace(visit);
            }
            for value in self.wasm_memory_objects.borrow().values() {
                value.trace(visit);
            }
            for value in self.wasm_memory_buffers.borrow().values() {
                value.trace(visit);
            }
            for value in self.wasm_table_objects.borrow().values() {
                value.trace(visit);
            }
            for value in self.wasm_global_objects.borrow().values() {
                value.trace(visit);
            }
            for (_, (value, _)) in self.wasm_host_functions.borrow().iter() {
                value.trace(visit);
            }
            for value in self.wasm_tag_objects.borrow().values() {
                value.trace(visit);
            }
            for value in self.wasm_js_exceptions.borrow().values() {
                value.trace(visit);
            }
            for value in self.wasm_extern_values.borrow().values() {
                value.trace(visit);
            }
        }
        // B5.4: the cached constructor boilerplate maps keep the shapes alive.
        for cell in self.construct_maps.iter() {
            if let Some(cell) = cell.get() {
                cell.trace(visit);
            }
        }
        // Cut 73: the cached ordinary-function boilerplate shapes.
        for map in self.function_boilerplate_maps.iter() {
            map.get().trace(visit);
        }
        if let Some(realm) = self.iter_result_realm.get() {
            realm.trace(visit);
        }
        if let Some(map) = self.iter_result_map.get() {
            map.trace(visit);
        }
        if let Some(methods) = self.generator_methods.get() {
            methods.trace(visit);
        }
        self.intl_number_format_data.trace(visit);
        self.intl_plural_rules_data.trace(visit);
        self.intl_date_time_format_data.trace(visit);
        self.intl_collator_data.trace(visit);
        self.intl_segments_data.trace(visit);
        self.intl_segment_iterator_data.trace(visit);
        self.temporal_data.trace(visit);
        self.temporal_calendars.trace(visit);
        for (value, text, _, _, _) in self.regexp_string_iter_data.borrow().values() {
            value.trace(visit);
            text.trace(visit);
        }
        for (text, _) in self.string_iter_data.borrow().values() {
            text.trace(visit);
        }
        self.global_symbol_registry.trace(visit);
        self.async_from_sync_continuations.trace(visit);
        self.error_stack.trace(visit);
        self.current_task_promise.trace(visit);
        // GC-4: WeakRef targets are held weakly — the table is deliberately
        // *not* traced, so a target dies unless reachable elsewhere (deref
        // then returns `undefined`; the compaction clears the entry). The
        // KeepDuringJob set (deref results of the current job) is traced.
        if let Ok(kept) = self.kept_during_job.try_borrow() {
            kept.trace(visit);
        } else {
            crux::heap::note_aborted_trace();
            return;
        }
        // GC-4: FinalizationRegistry — the cleanup callback is a strong
        // edge; each cell's held value is an ephemeron on its target (it
        // lives while the target does and is captured into a cleanup job
        // when the target dies); the unregister token is held weakly. The
        // compaction drops dead-target cells and clears dead tokens.
        if let Ok(registries) = self.finalization_registries.try_borrow() {
            for data in registries.values() {
                let Ok(data) = data.try_borrow() else {
                    crux::heap::note_aborted_trace();
                    return;
                };
                data.callback.trace(visit);
                for cell in &data.cells {
                    let Some(target) = value_box_addr(&cell.target) else {
                        continue;
                    };
                    let held = value_box_addr(&cell.held_value).unwrap_or(target);
                    crux::heap::note_ephemeron(target, held);
                }
            }
        } else {
            crux::heap::note_aborted_trace();
            return;
        }
        // `host_modules` is keyed by JsString: keys are heap edges too, so
        // trace the whole cell manually (the generic HashMap trace visits
        // values only).
        let Ok(host_modules) = self.host_modules.try_borrow() else {
            crux::heap::note_aborted_trace();
            return;
        };
        for (key, value) in host_modules.iter() {
            key.trace(visit);
            value.trace(visit);
        }
        self.module_namespaces.trace(visit);
        self.deferred_namespaces.trace(visit);
        self.module_sources.trace(visit);
        self.import_namespace_resolvers.trace(visit);
        self.deferred_module_waits.trace(visit);
        self.deferred_module_thens.trace(visit);
        self.module_eval_stack.trace(visit);
        self.entered_realm.trace(visit);
        self.async_from_sync.trace(visit);
        self.async_generator_awaits.trace(visit);
        self.iterator_helpers.trace(visit);
        self.wrapped_iterators.trace(visit);
        self.async_iterator_helpers.trace(visit);
        self.async_iterator_awaits.trace(visit);
        self.async_iterator_eager.trace(visit);
        self.async_iterator_pending.trace(visit);
        self.disposable_stacks.trace(visit);
        self.disposable_async_drivers.trace(visit);
        self.disposable_async_caps.trace(visit);
        self.async_body_disposal.trace(visit);
        for (state, _) in self.array_from_async.borrow().values() {
            state.trace(visit);
        }
        for (value, _, _) in self.wait_async.borrow().values() {
            value.trace(visit);
        }
        if let Ok(map) = self.array_iter_data.try_borrow() {
            for (value, _, _, _) in map.values() {
                value.trace(visit);
            }
        }
        // Visited because a buffer's [[ArrayBufferDetachKey]] is a GC value;
        // the state held only scalars and a refcounted block until that field
        // existed, which is why this line is newer than the table.
        self.buffer_data.trace(visit);
        self.symbol_data.trace(visit);
        self.bigint_data.trace(visit);
        self.regexp_data.trace(visit);
        self.raw_json_data.trace(visit);
        self.map_data.trace(visit);
        self.set_data.trace(visit);
        // GC-3: WeakMap/WeakSet entries are ephemerons — the key (and thus
        // the entry) lives only while it is reachable from other roots, and
        // the value lives only while its key does. Register the edges for
        // the collector's fixpoint instead of tracing them strongly; the
        // post-collection compaction drops entries whose key was swept.
        if let Ok(table) = self.weak_map_data.try_borrow() {
            for cell in table.values() {
                let Ok(data) = cell.try_borrow() else {
                    crux::heap::note_aborted_trace();
                    return;
                };
                for entry in data.iter().flatten() {
                    let Some(key) = value_box_addr(&entry.0) else {
                        continue;
                    };
                    let value = value_box_addr(&entry.1).unwrap_or(key);
                    crux::heap::note_ephemeron(key, value);
                }
            }
        } else {
            crux::heap::note_aborted_trace();
            return;
        }
        if let Ok(table) = self.weak_set_data.try_borrow() {
            for cell in table.values() {
                let Ok(data) = cell.try_borrow() else {
                    crux::heap::note_aborted_trace();
                    return;
                };
                for element in data.iter().flatten() {
                    if let Some(element) = value_box_addr(element) {
                        crux::heap::note_ephemeron(element, element);
                    }
                }
            }
        } else {
            crux::heap::note_aborted_trace();
            return;
        }
        // The iterator records are plain tuples (no `Trace` impl), so their
        // one heap value is visited by hand.
        if let Ok(table) = self.map_iter_data.try_borrow() {
            for record in table.values() {
                let Ok(record) = record.try_borrow() else {
                    crux::heap::note_aborted_trace();
                    return;
                };
                record.0.trace(visit);
            }
        } else {
            crux::heap::note_aborted_trace();
            return;
        }
        let Ok(table) = self.set_iter_data.try_borrow() else {
            crux::heap::note_aborted_trace();
            return;
        };
        for record in table.values() {
            let Ok(record) = record.try_borrow() else {
                crux::heap::note_aborted_trace();
                return;
            };
            record.0.trace(visit);
        }
    }
}

pub struct Agent {
    /// The collector-visible root containers; see [`AgentRoots`].
    pub(crate) roots: Rc<AgentRoots>,
    /// A termination request (v8::Isolate::TerminateExecution). The request has
    /// to reach a running isolate from another thread while the isolate itself
    /// cannot leave its own, so it is an atomic rather than agent state. It is
    /// read at the points the engine already stops at — a loop's back edge, the
    /// compiled loop's probe, and a call.
    termination: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// A host's heap size limit (`v8::CreateParams::heap_limits`'s maximum), in
    /// bytes, measured against the arena's live-box footprint. `None` — no
    /// limit — is this engine's default, and what V8's own default maximum of 0
    /// spells.
    heap_limit: Option<usize>,
    /// The limit the host first asked for, which the callback is handed as
    /// V8's `initial_heap_limit`.
    initial_heap_limit: usize,
    /// The host's near-heap-limit callback, which answers the limit to continue
    /// with. At most one, because V8 keeps one: a host that registers a second
    /// removes the first, which is why only the last one can ever run.
    near_heap_limit: Option<NearHeapLimitCallback>,
    /// The IC caches shared by every Vm run in this agent (the global-var
    /// cells and the P3 member/element cells). They lived on the Vm before,
    /// so every function call and script evaluation started cold and
    /// re-zeroed ~900 bytes of cache; they are re-validated on every access
    /// (against the current realm's global object and each object's property
    /// vector), so sharing them across Vms — and across realms — is exact
    /// and warms a nested call to its caller's shapes.
    pub(crate) global_cells: [Option<(crux::AtomId, usize)>; crate::ir::GLOBAL_CELLS],
    pub(crate) member_cells: [Option<(u64, crux::AtomId, usize)>; crate::ir::MEMBER_CELLS],
    /// The write-side value cache (L1a warm-store fast path): (id, name,
    /// generation, slot) — "at this generation, `name` is an own writable
    /// data property of `id` at property-vector `slot`". A store whose (id,
    /// name, generation) match skips the full [[Set]] re-derivation and
    /// writes the recorded slot in place (the own writable data property
    /// shadows the whole chain — spec 7.3.3 — and every own-property
    /// mutation bumps the generation, so a match pins the slot's content).
    /// Index-only — no trace edges. Boxed per the Cut 27 lesson.
    pub(crate) member_write_cells:
        Box<[Option<crate::ir::MemberWriteCell>; crate::ir::MEMBER_WRITE_CELLS]>,
    /// The L2 shape-keyed store cells: (map id, name) -> slot/field for any
    /// ordinary object whose CURRENT map is `map_id` (see
    /// [`crate::ir::MemberMapWriteCell`]). Probed when the (id, name) cell
    /// misses — a store loop whose object working set exceeds
    /// `MEMBER_WRITE_CELLS` thrashes the identity table but shares one map,
    /// so the shape-keyed hit stays O(1). Index-only — no trace edges.
    pub(crate) member_write_map_cells:
        Box<[Option<crate::ir::MemberMapWriteCell>; crate::ir::MEMBER_MAP_WRITE_CELLS]>,
    /// The prototype-chain read cache (2026-09-01): (receiver id, name,
    /// receiver generation, the found property's vector slot, the walked
    /// links' (id, generation)) — a hit re-reads the found property LIVE
    /// (the chain cell caches the resolution, never the value), so a value
    /// write to the found link's own property is always observed. All-
    /// scalar — no trace edges. Boxed per the Cut 27 lesson.
    pub(crate) member_chain_cells:
        Box<[Option<crate::ir::MemberChainCell>; crate::ir::MEMBER_CELLS]>,
    /// The shape-keyed prototype-chain read cache (Cut 75): like
    /// `member_chain_cells` but keyed by the RECEIVER'S MAP id instead of
    /// its object id, so a per-iteration-fresh object (every `new`, every
    /// fresh-literal method read) shares one cell with every other object on
    /// the same map — the identity cell can never hit a fresh receiver, and
    /// each read otherwise pays a full spec [[Get]] (~500ns, measured
    /// 2026-09-08). The hit re-validates that the CURRENT receiver's map is
    /// still the recorded one AND that it does not own `name`
    /// (has_own_property_atom — authoritative for both the deferred
    /// map-field state and the materialized vector state), then walks the
    /// recorded links' (id, generation) exactly like the identity cell. `id`
    /// holds the map id; the hit ignores `generation`. All-scalar — no
    /// trace edges.
    pub(crate) member_chain_map_cells: Box<[crate::ir::MemberChainCell; crate::ir::MEMBER_CELLS]>,
    /// The array-length cache (Cut 35 slice 13): (id, generation, length) —
    /// a hit skips the borrow and number conversion the for-of fast path
    /// pays every step. Boxed per the Cut 27 lesson.
    pub(crate) array_length_cells:
        Box<[Option<crate::ir::ArrayLengthCell>; crate::ir::MEMBER_CELLS]>,
    /// The proto-keyed read-cell fallback (Cut 23): `(prototype id, name) →
    /// slot` — fresh objects (a constructor's new `this`) share the
    /// prototype's shape, so the slot cached for the prototype is validated
    /// against each object's own property vector on access (a divergent
    /// layout misses and re-resolves). Self-validating like `member_cells`.
    pub(crate) member_proto_cells: [Option<(u64, crux::AtomId, usize)>; crate::ir::MEMBER_CELLS],
    /// Part B, B5.2: map-keyed read-cell cache `(map_id, name) → slot` —
    /// the fast path for fresh objects whose map describes the property.
    /// Non-`Option` with an impossible-id `MemberMapCell::empty()` (map ids
    /// start at 1) so the compiled `GetMemberName` probe reads the
    /// `#[repr(C)]` cells directly at fixed offsets.
    pub(crate) member_map_cells: [crate::ir::MemberMapCell; crate::ir::MEMBER_CELLS],
    pub(crate) array_element_cells:
        [Option<(u64, u64, crux::AtomId, usize)>; crate::ir::MEMBER_CELLS],
    /// The write-side chain cache (Cut 22): "the chain from this prototype
    /// holds no accessor/non-writable for this key" — the key's own
    /// property is absent on the receiver, so the store defines on it
    /// directly. Re-validated against the chain links' generations.
    pub(crate) member_store_cells: [Option<crate::ir::MemberStoreCell>; crate::ir::MEMBER_CELLS],
    /// The per-array for-of fast verdict (Cut 27): (array id, array
    /// generation, prototype id) — "this array iterates by index" (plain
    /// Array, no own @@iterator, prototype is the stock %Array.prototype%
    /// whose iterator infrastructure the Cut 24 verdict certified). The
    /// array generation catches an own @@iterator addition and proto
    /// changes (Cut 22's mechanism bumps it); the prototype's own mutations
    /// are re-validated per access through `for_of_fast_probe`. A hit skips
    /// the own-property scan, the intrinsics lookups, and the proto walk
    /// the bench's 100k begins ran each time. Boxed so the 16-entry table
    /// does not bloat the Agent struct's hot-field cache footprint (an
    /// inline copy regressed the leaf-call path by ~10ns/call).
    pub(crate) for_of_array_cells: Box<[Option<(u64, u32, u64)>; crate::ir::MEMBER_CELLS]>,
    /// The per-array species fast verdict: (array id, array generation,
    /// prototype id) — "this array is a plain stock Array whose chain
    /// `constructor` resolves to its realm's %Array%". The array generation
    /// catches an own `constructor` addition and proto changes (Cut 22's
    /// mechanism bumps it); the shared verdict is re-validated per access
    /// through the species probe. A hit skips the own-property scan and the
    /// proto walk on every result-building call. Boxed so the 16-entry table
    /// does not bloat the Agent struct's hot-field cache footprint.
    pub(crate) species_array_cells: Box<[Option<(u64, u32, u64)>; crate::ir::MEMBER_CELLS]>,
    /// G15: the per-callee leaf-call record table the compiled call sites and
    /// the slow-path helpers address (`JitCallContext::leaf_records`). It lives
    /// on the agent rather than in the per-run JIT context so a run does not
    /// memset it — the table is large enough to give a megamorphic site one
    /// record per callee. Records carry `leaf_gen` (a code generation) so a run
    /// that may have evicted compiled code re-probes instead of using a freed
    /// entry; a sweeping collection clears the table (a recycled box address
    /// could otherwise match a record's identity).
    pub(crate) leaf_records: Box<[crate::jit::LeafCallRecord; crate::jit::LEAF_CALL_RECORDS]>,
    /// G15: the code generation the agent's leaf-call records carry. Bumped at
    /// the start of every top-level JIT run (whose lookup may have evicted
    /// compiled code), so records left by an earlier run never match.
    pub(crate) leaf_gen: u32,
    /// The builtin-call verdict cache (Cut 81's handler cell, widened to the
    /// full direct-dispatch decision): function id → the handler to run, the
    /// crux-native "run its own closure" verdict, or neither. Filling it
    /// skips BOTH the thread-local `BUILTIN_HANDLERS` get and the agent's
    /// `builtin_dispatch_cache` HashMap get on a warm builtin call — the two
    /// SipHash lookups that dominated the JIT's `Math.abs` row. Boxed per the
    /// Cut 27 lesson.
    pub(crate) builtin_call_cells:
        Box<[Option<(u64, crate::function::BuiltinCall)>; crate::ir::BUILTIN_HANDLER_CELLS]>,
    /// The installed-builtin CONSTRUCT handler cache (the construct-side
    /// mirror of the handler verdict `builtin_call_cells` carries): function
    /// id → the registered agent-dependent constructor, so a warm `new` skips
    /// the thread-local `CONSTRUCT_HANDLERS` RefCell<HashMap> get. Boxed per
    /// the Cut 27 lesson.
    pub(crate) builtin_ctor_cells:
        Box<[Option<(u64, crate::function::BuiltinCtor)>; crate::ir::BUILTIN_HANDLER_CELLS]>,
    /// The free-list of Vms for per-call reuse: `run_compiled_body`, the
    /// construct fast path, and the script/eval paths take one, run, and
    /// return it — a pooled Vm is never handed to a suspended
    /// generator/async state (those own their Vm). Boxed so taking and
    /// returning a Vm moves only the pointer: `Vm` is ~1.1KB, and the
    /// by-value pool copies dominated the non-leaf call path's profile
    /// (memcpy 37.6%, non-leaf calls -38% once boxed). `vec_box` would
    /// restore exactly that cost, so it is allowed on [`AgentRoots`].
    ///
    /// The pool itself lives in [`AgentRoots`]: its Vms' heap-buffered value
    /// stacks are roots.
    /// The installed JIT hook (the `jit` crate's `install`), consulted by
    /// the leaf-call path before interpreting a certified body. The Drop
    /// impl frees the installed cache.
    pub jit_hook: Option<crate::jit::JitHook>,
    /// The number of compiled JIT bodies currently executing on the native
    /// stack. The recursion guard for the JIT's private per-call frame
    /// buffers (`runtime::jit::MAX_JIT_DEPTH`), and the cache's in-flight
    /// signal: eviction is disabled while any frame runs.
    pub jit_depth: usize,
    /// The heap this agent allocates in, when a host owns the arena (an
    /// isolate under the `v8` bridge). Null for a bare agent, whose heap is the
    /// thread's own.
    ///
    /// A collection is driven by the agent, not by the thread: it can run at a
    /// job boundary on a thread that has no isolate entered. Without this the
    /// collector would pick the thread's own arena and mark the agent's roots
    /// into it, which corrupts the arena that owns them.
    heap_state: std::cell::Cell<*const crux::heap::HeapState>,
    /// Host-defined operations (spec's host hooks); `None` uses the
    /// spec's default implementations.
    pub host_hooks: Option<Box<dyn HostHooks>>,
    /// [[LittleEndian]]: the host byte order used by GetValueFromBuffer.
    pub little_endian: bool,
    /// [[CanBlock]]: false for the main thread; Atomics.wait joins in
    /// Phase 17.
    pub can_block: bool,
    /// [[Signifier]]: globally unique per agent.
    pub signifier: u64,
    /// [[IsLockFree1/2/8]]: whether atomic ops of those sizes are lock-free.
    pub is_lock_free: [bool; 3],
    /// GC-2: a build-in-progress flag — native code accumulating
    /// handle-bearing values in local buffers (the class element build) sets
    /// it for the window; a `--gc-stress` collection that fires inside then
    /// aborts (retain everything), so the half-built buffers cannot be swept
    /// out from under the final record assignment.
    pub build_roots: std::cell::Cell<bool>,
    /// [[ModuleAsyncEvaluationCount]]: module linking (Phase 7).
    pub module_async_evaluation_count: u32,
    /// The two handlers `WebAssembly.compileStreaming` attaches to the source
    /// promise, keyed by function identity: a Rust-backed builtin carries no
    /// data of its own, so the stream it settles is found here. An entry is
    /// consumed when the handler runs, which is why the stream roots its own
    /// values (see `api::WasmStreaming`).
    /// The `disposeAsync` continuations, keyed by function identity: the
    /// driver id and whether the awaited disposal promise rejected.
    pub disposable_async_cont: std::collections::HashMap<u64, (u64, bool)>,
    /// The async-body disposal continuations, keyed by function identity:
    /// the driver id and whether the awaited disposal promise rejected.
    pub async_body_disposal_cont: std::collections::HashMap<u64, (u64, bool)>,
    /// Agent-dependent built-ins (`%eval%`-pattern functions that cannot run
    /// inside crux closures) dispatch by intrinsic identity in
    /// `function::call_inner`. The result of that linear chain is memoized
    /// per function id: `0` means no agent dispatch (a plain closure
    /// builtin), otherwise the index into the dispatch table.
    pub builtin_dispatch_cache: IdMap<u8>,
    /// DeferredModule `.then` continuations (import-defer): each waiter
    /// function id maps to (wait id, is-rejection); the wait state holds the
    /// remaining async-dependency countdown and the capability.
    pub deferred_module_waiter_fns: std::collections::HashMap<u64, (u64, bool)>,
    /// [[BooleanData]] of Boolean wrapper objects, keyed by object identity
    /// (spec 20.3.1: `new Boolean(v)` boxes the ToBoolean result).
    pub boolean_data: std::collections::HashMap<u64, bool>,
    /// [[NumberData]] of Number wrapper objects, keyed by object identity
    /// (spec 21.1.1: `new Number(v)` boxes the ToNumber result).
    pub number_data: std::collections::HashMap<u64, f64>,
    /// [[DateValue]] of Date instances, keyed by object identity (spec
    /// 21.4.3: ms since the epoch).
    pub date_data: IdMap<f64>,
    /// The [[InitializedLocale]] records of Intl.Locale instances, keyed by
    /// object identity (ECMA-402 §15: the canonical locale string).
    pub intl_locale_data: std::collections::HashMap<u64, crate::builtins::intl::IntlLocaleRecord>,
    /// The per-instance bound `format` functions: function id → the
    /// NumberFormat instance's object id (ECMA-402 §16.5.2 [[NumberFormat]]).
    pub intl_format_functions: std::collections::HashMap<u64, u64>,
    /// The [[InitializedRelativeTimeFormat]] records of
    /// Intl.RelativeTimeFormat instances, keyed by object identity
    /// (ECMA-402 §18: locale, style, numeric, numbering system).
    pub intl_rtf_data: std::collections::HashMap<
        u64,
        crate::builtins::intl::relative_time_format::RelativeTimeFormatRecord,
    >,
    /// The [[InitializedListFormat]] records of Intl.ListFormat instances,
    /// keyed by object identity (ECMA-402 §14: locale, type, style).
    pub intl_list_format_data:
        std::collections::HashMap<u64, crate::builtins::intl::list_format::ListFormatRecord>,
    /// The [[InitializedDisplayNames]] records of Intl.DisplayNames
    /// instances, keyed by object identity (ECMA-402 §12).
    pub intl_display_names_data:
        std::collections::HashMap<u64, crate::builtins::intl::display_names::DisplayNamesRecord>,
    /// The per-instance bound `format` functions: function id → the
    /// DateTimeFormat instance's object id (ECMA-402 §11.3.3).
    pub intl_dtf_format_functions: std::collections::HashMap<u64, u64>,
    /// The per-instance bound `compare` functions: function id → the
    /// Collator instance's object id (ECMA-402 §10.3.3).
    pub intl_collator_compare_functions: std::collections::HashMap<u64, u64>,
    /// The [[InitializedSegmenter]] records of Intl.Segmenter instances,
    /// keyed by object identity (ECMA-402 §19: locale, granularity).
    pub intl_segmenter_data:
        std::collections::HashMap<u64, crate::builtins::intl::segmenter::SegmenterRecord>,
    /// The [[InitializedDurationFormat]] records of Intl.DurationFormat
    /// instances, keyed by object identity (ECMA-402 §13: locale,
    /// numberingSystem, style, the per-unit options, fractionalDigits).
    pub intl_duration_format_data: std::collections::HashMap<
        u64,
        crate::builtins::intl::duration_format::DurationFormatRecord,
    >,
    /// Compiled regexp programs, keyed by source then flags. A `RegExp`
    /// literal creates a fresh object on every evaluation (spec 13.2.8), but
    /// its program is immutable and identical each time, so the compiled form
    /// is shared (see `compile_regexp`).
    pub(crate) compiled_regexps:
        std::collections::HashMap<Vec<u16>, Vec<(regexp::Flags, std::rc::Rc<regexp::Regex>)>>,
    /// [[ErrorData]] markers of Error instances, keyed by object identity
    /// (spec 20.5.4: `Error.isError` and the `[object Error]` tag need it).
    pub error_data: std::collections::HashSet<u64>,
    /// The compiled module of `WebAssembly.Module` instances, keyed by object
    /// identity (JS-API spec: the [[Module]] internal slot; Wave 2 of Cut 10).
    #[cfg(feature = "wasm")]
    pub wasm_modules: std::collections::HashMap<u64, wasm::Module>,
    /// The shared wasm engine store every `WebAssembly.Instance` in this
    /// agent instantiates into (so imports alias cells across instances).
    #[cfg(feature = "wasm")]
    pub wasm_store: std::cell::RefCell<wasm::Store>,
    /// Exported-function wrappers: function id -> engine (instance, index).
    #[cfg(feature = "wasm")]
    pub wasm_exports: std::collections::HashMap<u64, (usize, usize)>,
    /// The memory-cell registry of `WebAssembly.Memory` wrapper objects:
    /// wrapper object id -> engine store cell (Cut 10 wave 3b).
    #[cfg(feature = "wasm")]
    pub wasm_memories: std::collections::HashMap<u64, usize>,
    /// The table-cell registry of `WebAssembly.Table` wrapper objects: wrapper
    /// object id -> engine store cell (Cut 10 wave 3b).
    #[cfg(feature = "wasm")]
    pub wasm_tables: std::collections::HashMap<u64, usize>,
    /// The global-cell registry of `WebAssembly.Global` wrapper objects:
    /// wrapper object id -> engine store cell (Cut 10 wave 3b).
    #[cfg(feature = "wasm")]
    pub wasm_globals: std::collections::HashMap<u64, usize>,
    /// The next external-host token (monotonic within the agent).
    #[cfg(feature = "wasm")]
    pub wasm_host_seq: u64,
    /// The tag-cell registry of `WebAssembly.Tag` wrapper objects: wrapper
    /// object id -> engine store cell (Cut 10 wave 4).
    #[cfg(feature = "wasm")]
    pub wasm_tags: std::collections::HashMap<u64, usize>,
    /// The engine tag cell behind `WebAssembly.JSTag` (the tag arbitrary JS
    /// exception values enter wasm with), once materialized: JS throws wrap
    /// in it, a wasm `catch` for it intercepts them, and an escape unwraps
    /// the payload back to the JS value (Cut 10 wave 4).
    #[cfg(feature = "wasm")]
    pub wasm_js_tag_cell: Option<usize>,
    /// The payload of `WebAssembly.Exception` objects, keyed by object id:
    /// the exception's tag cell and its arguments as engine values (Cut 10
    /// wave 4).
    #[cfg(feature = "wasm")]
    pub wasm_exceptions: std::collections::HashMap<u64, (usize, Vec<wasm::Value>)>,
    /// The next externref host token (monotonic within the agent).
    #[cfg(feature = "wasm")]
    pub wasm_extern_seq: u32,
    /// The nesting depth of class field initializers currently evaluating.
    /// A direct eval inside one applies the "Eval Inside Initializer" early
    /// errors (spec 19.2.1.1): `arguments` is a SyntaxError there.
    pub field_initializer_depth: usize,
    /// The realm count as a plain `Cell` (Cut 35 slice 25): the leaf-inline
    /// eligibility checks read `realms.len()` on every call, and the
    /// RefCell borrow was measurable there; `realms` is only ever pushed
    /// (via `initialize_host_defined_realm`) and never popped or cleared,
    /// so a count cell stays exact, and `Cell` permits the write through
    /// the `&Agent` like `RefCell` does.
    pub realm_count: std::cell::Cell<usize>,
    /// GC-1 slice 3: collect at every safe point when set (the `--gc-stress`
    /// mode; .notes/gc-plan.md GC-2 hardens the root audit under it).
    pub gc_stress: Cell<bool>,
    /// A5: the young-cohort size that paces a minor collection at a safe point.
    /// A `Cell` rather than the constant alone so `--nursery-threshold` and
    /// `--nursery-stress` can tune it without a rebuild (the tuning cut's whole
    /// point).
    pub nursery_threshold: Cell<usize>,
    /// The live-box count after the last *major* collection, for the growth
    /// threshold that decides when a safe point collects. Minors deliberately
    /// do not update it: it is the old generation's baseline (a minor promotes
    /// its survivors, so a fresh baseline would let the old generation grow
    /// without ever triggering the major that reclaims it).
    pub last_collected_live: Cell<usize>,
    /// Hot constructor property patterns (Cut 35 slice 30): function id →
    /// `(count, array)` of the property names that constructor body assigns
    /// to `this`. Direct-mapped (a SipHash HashMap lookup per construct
    /// was measurable); re-validated by the function id. Used by
    /// `construct_this_object` to pre-warm the member store cache so the
    /// first `this.x =` hit the cache on the first construct, not the
    /// second (V8's AllocationSite / boilerplate approach). Boxed per the
    /// Cut 27 lesson.
    pub(crate) construct_property_patterns:
        Box<[Option<crate::ir::ConstructPatternCell>; crate::ir::CONSTRUCT_PATTERN_CELLS]>,
    /// The host's collection observers, in registration order
    /// (`crux::heap::GcObserver`). Taken out for the duration of a collection
    /// so one that runs host code cannot re-enter this list — a nested
    /// collection then runs none, which is also V8's shape: a collection never
    /// starts inside one.
    collection_observers: RefCell<Vec<Box<dyn crux::heap::GcObserver>>>,
}

/// A microtask queue a host owns and drains itself (`v8::MicrotaskQueue`).
///
/// The realm a context was created with it sends its promise jobs here instead
/// of the agent's own queue, so the host decides when that context's async work
/// runs — which is the whole of the feature: a vm context's continuations must
/// not join the outer context's turn. The policy is the queue's own: `Explicit`
/// is drained only by the host's `PerformCheckpoint`, `Auto` also by the
/// isolate's run.
pub(crate) struct MicrotaskQueue {
    policy: crate::api::MicrotasksPolicy,
    jobs: VecDeque<Job>,
}

impl Drop for Agent {
    fn drop(&mut self) {
        // The JIT cache is a heap allocation owned by the installed hook
        // (`jit::install` `Box::into_raw`'d it); free it here.
        if let Some(hook) = self.jit_hook {
            // SAFETY: the agent is the cache's sole owner and is being
            // dropped, so this runs exactly once with a live pointer.
            unsafe { (hook.drop_cache)(hook.cache) };
        }
        unregister_live_agent(&self.roots, self.signifier);
    }
}

/// Undoes the heap entry a collection installed for its agent.
struct InstalledHeap;

impl Drop for InstalledHeap {
    fn drop(&mut self) {
        crux::heap::leave();
    }
}

/// A3/A5: the default young-cohort size that paces a minor collection at a safe
/// point. Small enough that the cohort's reclamation trails allocation closely,
/// large enough that a minor is not run for a handful of boxes. `--nursery-
/// threshold` and `--nursery-stress` override it per agent.
const DEFAULT_NURSERY_THRESHOLD: usize = 8192;

/// The error a check point throws when a termination request is outstanding:
/// V8's termination exception, which is a plain `Error` and not a NativeError.
pub fn termination_error() -> JsError {
    JsError::new(ErrorKind::Error, "execution terminated".into())
}

impl Agent {
    /// Tell this agent which heap it allocates in. Called by the host that
    /// owns the arena; a bare agent keeps the thread's own.
    ///
    /// # Safety
    ///
    /// `state` must stay alive for as long as this agent does.
    pub unsafe fn set_heap_state(&self, state: *const crux::heap::HeapState) {
        self.heap_state.set(state);
    }

    /// The flag a termination request sets, for a handle that has to cross
    /// threads to reach it (v8::IsolateHandle).
    pub fn termination_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.termination)
    }

    /// Ask a running execution to stop (v8::Isolate::TerminateExecution).
    pub fn request_termination(&self) {
        self.termination
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Withdraw the request (v8::Isolate::CancelTerminateExecution).
    pub fn cancel_termination(&self) {
        self.termination
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a request is outstanding and not yet withdrawn.
    pub fn is_terminating(&self) -> bool {
        self.termination.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The heap limit a host asked for, if any
    /// (`v8::CreateParams::heap_limits`).
    pub fn set_heap_limits(&mut self, initial: usize, maximum: usize) {
        self.initial_heap_limit = initial;
        // V8's default maximum is 0, which means "no limit": a host that asks
        // for 0 must not be given one at zero bytes, which would fire the
        // callback on the first allocation.
        self.heap_limit = (maximum > 0).then_some(maximum);
    }

    /// The limit in force, if any. A host reads its own limit back through this
    /// (`v8::Isolate::GetHeapStatistics`'s `heap_size_limit`), and the
    /// callback's answer replaces it.
    pub fn heap_limit(&self) -> Option<usize> {
        self.heap_limit
    }

    /// Install the host's near-heap-limit callback
    /// (`v8::Isolate::AddNearHeapLimitCallback`). A replacement is the whole of
    /// "a second callback": see the field.
    pub fn set_near_heap_limit_callback(&mut self, callback: Option<NearHeapLimitCallback>) {
        self.near_heap_limit = callback;
    }

    /// Withdraw the callback and restore the limit the host named
    /// (`v8::Isolate::RemoveNearHeapLimitCallback`), where 0 is "leave the limit
    /// alone" — which is what `deno_core` passes.
    pub fn clear_near_heap_limit_callback(&mut self, heap_limit: usize) {
        self.near_heap_limit = None;
        if heap_limit > 0 {
            self.heap_limit = Some(heap_limit);
        }
    }

    /// Fire the host's callback when the live heap has reached the limit it set
    /// — this engine's substitute for a V8 out-of-memory, which it cannot have
    /// because its arena grows instead. Read at every budget crossing, after
    /// that trigger's collections, so a collection that reclaimed enough does
    /// not fire it. A host that terminates the execution inside its callback
    /// stops the run at the next check point; one that answers a larger limit
    /// keeps it going.
    fn check_heap_limit(&mut self) {
        let Some(limit) = self.heap_limit else {
            return;
        };
        let Some(callback) = self.near_heap_limit.as_mut() else {
            return;
        };
        let live = crux::heap::with_heap(|heap| heap.allocated_bytes());
        if live < limit {
            return;
        }
        // The callback gets no isolate, so it cannot re-enter the engine; the
        // borrow below is what that shape buys.
        let next = callback(limit, self.initial_heap_limit);
        self.heap_limit = Some(next);
    }

    pub fn new() -> Self {
        crate::function::ensure_ecma_hook();
        // Built before the struct so the wasm store can share the very flag a
        // host's `IsolateHandle` sets: one request, two engines reading it.
        let termination = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self {
            termination: std::sync::Arc::clone(&termination),
            heap_limit: None,
            initial_heap_limit: 0,
            near_heap_limit: None,
            global_cells: [None; crate::ir::GLOBAL_CELLS],
            member_cells: [None; crate::ir::MEMBER_CELLS],
            member_write_cells: {
                // The write table is larger than the other IC tables; build it
                // on the heap directly (a `from_fn` array temporary would sit on
                // the stack — ~128KB at MEMBER_WRITE_CELLS=4096 — and overflow a
                // small-stack embedding context).
                let mut cells: Vec<Option<crate::ir::MemberWriteCell>> =
                    Vec::with_capacity(crate::ir::MEMBER_WRITE_CELLS);
                cells.resize_with(crate::ir::MEMBER_WRITE_CELLS, || None);
                cells
                    .into_boxed_slice()
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("sized exactly MEMBER_WRITE_CELLS"))
            },
            member_write_map_cells: {
                // Same heap-direct construction as the identity write table
                // (the boxed array must not sit on a small native stack).
                let mut cells: Vec<Option<crate::ir::MemberMapWriteCell>> =
                    Vec::with_capacity(crate::ir::MEMBER_MAP_WRITE_CELLS);
                cells.resize_with(crate::ir::MEMBER_MAP_WRITE_CELLS, || None);
                cells
                    .into_boxed_slice()
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("sized exactly MEMBER_MAP_WRITE_CELLS"))
            },
            member_chain_cells: Box::new(std::array::from_fn(|_| None)),
            member_chain_map_cells: Box::new(
                [const { crate::ir::MemberChainCell::empty() }; crate::ir::MEMBER_CELLS],
            ),
            array_length_cells: Box::new(std::array::from_fn(|_| None)),
            member_proto_cells: [None; crate::ir::MEMBER_CELLS],
            member_map_cells: [const { crate::ir::MemberMapCell::empty() };
                crate::ir::MEMBER_CELLS],
            array_element_cells: [None; crate::ir::MEMBER_CELLS],
            member_store_cells: [None; crate::ir::MEMBER_CELLS],
            for_of_array_cells: Box::new([None; crate::ir::MEMBER_CELLS]),
            species_array_cells: Box::new([None; crate::ir::MEMBER_CELLS]),
            leaf_records: Box::new(
                [crate::jit::LeafCallRecord::empty(); crate::jit::LEAF_CALL_RECORDS],
            ),
            leaf_gen: 0,
            builtin_call_cells: Box::new(std::array::from_fn(|_| None)),
            builtin_ctor_cells: Box::new(std::array::from_fn(|_| None)),
            construct_property_patterns: Box::new(std::array::from_fn(|_| None)),
            collection_observers: RefCell::new(Vec::new()),
            jit_hook: None,
            jit_depth: 0,
            roots: Rc::new(AgentRoots::new()),
            heap_state: std::cell::Cell::new(std::ptr::null()),
            host_hooks: None,
            little_endian: cfg!(target_endian = "little"),
            can_block: false,
            signifier: NEXT_AGENT_ID.fetch_add(1, Ordering::Relaxed),
            is_lock_free: [
                is_lock_free_for_size(1),
                is_lock_free_for_size(2),
                is_lock_free_for_size(8),
            ],
            build_roots: std::cell::Cell::new(false),
            module_async_evaluation_count: 0,
            disposable_async_cont: std::collections::HashMap::new(),
            async_body_disposal_cont: std::collections::HashMap::new(),
            builtin_dispatch_cache: std::collections::HashMap::default(),
            deferred_module_waiter_fns: std::collections::HashMap::new(),
            boolean_data: std::collections::HashMap::new(),
            number_data: std::collections::HashMap::new(),
            date_data: std::collections::HashMap::default(),
            intl_locale_data: std::collections::HashMap::new(),
            intl_format_functions: std::collections::HashMap::new(),
            intl_rtf_data: std::collections::HashMap::new(),
            intl_list_format_data: std::collections::HashMap::new(),
            intl_display_names_data: std::collections::HashMap::new(),
            intl_dtf_format_functions: std::collections::HashMap::new(),
            intl_collator_compare_functions: std::collections::HashMap::new(),
            intl_segmenter_data: std::collections::HashMap::new(),
            intl_duration_format_data: std::collections::HashMap::new(),
            compiled_regexps: std::collections::HashMap::new(),
            error_data: std::collections::HashSet::new(),
            #[cfg(feature = "wasm")]
            wasm_modules: std::collections::HashMap::new(),
            #[cfg(feature = "wasm")]
            wasm_store: std::cell::RefCell::new(wasm::Store::with_termination(
                std::sync::Arc::clone(&termination),
            )),
            #[cfg(feature = "wasm")]
            wasm_exports: std::collections::HashMap::new(),
            #[cfg(feature = "wasm")]
            wasm_memories: std::collections::HashMap::new(),
            #[cfg(feature = "wasm")]
            wasm_tables: std::collections::HashMap::new(),
            #[cfg(feature = "wasm")]
            wasm_globals: std::collections::HashMap::new(),
            #[cfg(feature = "wasm")]
            wasm_host_seq: 0,
            #[cfg(feature = "wasm")]
            wasm_tags: std::collections::HashMap::new(),
            #[cfg(feature = "wasm")]
            wasm_js_tag_cell: None,
            #[cfg(feature = "wasm")]
            wasm_exceptions: std::collections::HashMap::new(),
            #[cfg(feature = "wasm")]
            wasm_extern_seq: 0,
            field_initializer_depth: 0,
            realm_count: std::cell::Cell::new(0),
            gc_stress: Cell::new(false),
            nursery_threshold: Cell::new(DEFAULT_NURSERY_THRESHOLD),
            last_collected_live: Cell::new(0),
        }
    }

    /// The running execution context: the top of the stack. Invariant: the
    /// stack is never empty after `initialize_host_defined_realm` has run.
    pub fn running_context(&self) -> Result<Ref<'_, ExecutionContext>, JsError> {
        self.roots.execution_context_stack.last().ok_or_else(|| {
            JsError::new(
                ErrorKind::ReferenceError,
                "No running execution context".into(),
            )
        })
    }

    pub fn running_context_mut(&self) -> Result<RefMut<'_, ExecutionContext>, JsError> {
        self.roots
            .execution_context_stack
            .last_mut()
            .ok_or_else(|| {
                JsError::new(
                    ErrorKind::ReferenceError,
                    "No running execution context".into(),
                )
            })
    }

    /// A Vm for a new run: a pooled one reset for this env/strict, or a
    /// fresh allocation when the pool is empty.
    pub(crate) fn take_vm(
        &mut self,
        lexical_env: crate::env::EnvRef,
        strict: bool,
    ) -> Box<crate::ir::Vm> {
        match self.roots.vm_pool.borrow_mut().pop() {
            // `return_vm` fully reset the pooled Vm before it was pooled, so
            // only the per-run env and strictness need re-pointing.
            Some(mut vm) => {
                vm.rebind(lexical_env, strict);
                vm
            }
            None => Box::new(crate::ir::Vm::new(lexical_env, strict)),
        }
    }

    /// Return a Vm to the pool after a leaf run completed normally: the leaf
    /// machinery has already restored its persistent state, so only the
    /// handful of fields a leaf can leave dirty are dropped (see
    /// [`Vm::clean_after_leaf`]) instead of the full `reset`. An error path
    /// must keep using [`return_vm`](Self::return_vm) — an unwind can leave
    /// the value stack past the call site.
    pub(crate) fn return_leaf_vm(&mut self, mut vm: Box<crate::ir::Vm>) {
        vm.clean_after_leaf();
        self.roots.vm_pool.borrow_mut().push(vm);
    }

    /// Return a Vm to the pool after its run finished (the next run reuses
    /// its Vec capacities and inline frame).
    pub(crate) fn return_vm(&mut self, mut vm: Box<crate::ir::Vm>) {
        // GC-4: the pooled Vm must not trace the finished run's stale state —
        // a frame slot, value-stack entry, or scope-env binding holding a
        // WeakRef/FinalizationRegistry target would keep it alive until the
        // next run resets the Vm. So the Vm is ALWAYS fully reset before it
        // is pooled; `take_vm` then only has to re-point the env and
        // strictness for the next run (a pooled Vm is otherwise clean).
        match self.running_context() {
            Ok(context) => {
                let env = context.lexical_environment;
                vm.reset(env, false);
            }
            // No running context (defensive; every caller has one): reset in
            // place, keeping the Vm's own env, so the pooled Vm is still clean.
            Err(_) => vm.reset_for_pool(),
        }
        self.roots.vm_pool.borrow_mut().push(vm);
    }

    /// The record of the ECMAScript function `id`, when it is registered.
    ///
    /// A guard, not a reference: the table is in [`AgentRoots`]. Keep it to
    /// the statements that copy the fields out — see the field's own note.
    pub fn ecma_function(&self, id: &u64) -> Option<Ref<'_, crate::function::EcmaFunction>> {
        Ref::filter_map(self.roots.ecma_functions.borrow(), |table| table.get(id)).ok()
    }

    /// The mutable counterpart of [`ecma_function`](Self::ecma_function).
    pub(crate) fn ecma_function_mut(
        &self,
        id: &u64,
    ) -> Option<RefMut<'_, crate::function::EcmaFunction>> {
        RefMut::filter_map(self.roots.ecma_functions.borrow_mut(), |table| {
            table.get_mut(id)
        })
        .ok()
    }

    /// Read the whole record table. Enumeration is a test's job today — the
    /// snapshot writer resolves records one at a time by id — so it is
    /// test-only, like the accessors that check a table's shape.
    #[cfg(test)]
    pub(crate) fn with_ecma_functions<T>(
        &self,
        read: impl FnOnce(&IdMap<crate::function::EcmaFunction>) -> T,
    ) -> T {
        read(&self.roots.ecma_functions.borrow())
    }

    pub(crate) fn register_ecma_function(&self, id: u64, data: crate::function::EcmaFunction) {
        self.roots.ecma_functions.borrow_mut().insert(id, data);
    }

    pub(crate) fn remove_ecma_function(&self, id: &u64) -> Option<crate::function::EcmaFunction> {
        self.roots.ecma_functions.borrow_mut().remove(id)
    }

    /// The [[MapData]] of `id`, when the object is a Map. The entry is an
    /// `Rc`-shared cell, so this hands the collection over and leaves the
    /// table unborrowed.
    pub(crate) fn map_collection(&self, id: u64) -> Option<Rc<RefCell<MapCollection>>> {
        self.roots.map_data.borrow().get(&id).cloned()
    }

    /// The [[SetData]] of `id`, when the object is a Set; see
    /// [`map_collection`](Self::map_collection).
    pub(crate) fn set_collection(&self, id: u64) -> Option<Rc<RefCell<SetCollection>>> {
        self.roots.set_data.borrow().get(&id).cloned()
    }

    /// The [[WeakMapData]] entry list of `id`, when the object is a WeakMap.
    pub fn weak_map_entries(&self, id: u64) -> Option<Rc<RefCell<Vec<MapEntry>>>> {
        self.roots.weak_map_data.borrow().get(&id).cloned()
    }

    /// The [[WeakSetData]] entry list of `id`, when the object is a WeakSet.
    pub fn weak_set_entries(&self, id: u64) -> Option<Rc<RefCell<Vec<SetEntry>>>> {
        self.roots.weak_set_data.borrow().get(&id).cloned()
    }

    /// The [[IteratedMap]] record of `id`, when the object is a Map iterator.
    pub fn map_iter_record(&self, id: u64) -> Option<Rc<RefCell<IterRecord>>> {
        self.roots.map_iter_data.borrow().get(&id).cloned()
    }

    /// The [[IteratedSet]] record of `id`, when the object is a Set iterator.
    pub fn set_iter_record(&self, id: u64) -> Option<Rc<RefCell<IterRecord>>> {
        self.roots.set_iter_data.borrow().get(&id).cloned()
    }

    /// A snapshot of a Map's [[MapData]]. The set-methods and the iterators
    /// read the list this way: no user code runs between the read and the
    /// use, so nothing needs the collection's guard held.
    pub fn map_entries(&self, id: u64) -> Option<Vec<MapEntry>> {
        Some(self.map_collection(id)?.borrow().entries.clone())
    }

    /// A snapshot of a Set's [[SetData]]; see [`map_entries`](Self::map_entries).
    pub fn set_entries(&self, id: u64) -> Option<Vec<SetEntry>> {
        Some(self.set_collection(id)?.borrow().entries.clone())
    }

    /// The [[DisposableStackData]] of a DisposableStack/AsyncDisposableStack.
    pub(crate) fn disposable_stack(
        &self,
        id: u64,
    ) -> Option<Rc<RefCell<crate::builtins::disposable::DisposableStackData>>> {
        self.roots.disposable_stacks.borrow().get(&id).cloned()
    }

    pub(crate) fn register_disposable_stack(
        &self,
        id: u64,
        data: crate::builtins::disposable::DisposableStackData,
    ) {
        self.roots
            .disposable_stacks
            .borrow_mut()
            .insert(id, Rc::new(RefCell::new(data)));
    }

    /// The global symbol registry (spec 20.4.2.1) as one guard: `Symbol.for`
    /// and the bridge both need to look a description up and insert it in the
    /// same borrow.
    pub fn symbol_registry(&self) -> std::cell::RefMut<'_, Vec<(JsString, crux::symbol::Symbol)>> {
        self.roots.global_symbol_registry.borrow_mut()
    }

    /// The Temporal record of `id`, when the object is a Temporal instance.
    /// A copy: the record is a small enum, and every reader wants it owned.
    pub(crate) fn temporal_record(
        &self,
        id: u64,
    ) -> Option<crate::builtins::temporal::TemporalRecord> {
        self.roots.temporal_data.borrow().get(&id).cloned()
    }

    /// The rendered stack of `id`, when it is an Error whose stack the engine
    /// captured. A copy: the record is a string and a frame list.
    pub(crate) fn error_stack(&self, id: u64) -> Option<ErrorStack> {
        self.roots.error_stack.borrow().get(&id).cloned()
    }

    /// The AsyncFromSyncIterator value-unwrap continuation of `id`.
    pub(crate) fn async_from_sync_continuation(
        &self,
        id: u64,
    ) -> Option<std::rc::Rc<crate::async_await::AsyncFromSyncContinuationEntry>> {
        self.roots
            .async_from_sync_continuations
            .borrow()
            .get(&id)
            .cloned()
    }

    /// The [[Cells]]/[[CleanupCallback]] record of a FinalizationRegistry.
    pub(crate) fn finalization_registry(
        &self,
        id: u64,
    ) -> Option<std::rc::Rc<RefCell<crate::builtins::weakref::FinalizationData>>> {
        self.roots
            .finalization_registries
            .borrow()
            .get(&id)
            .cloned()
    }

    /// Whether a host module was registered under `specifier`.
    pub fn has_host_module(&self, specifier: &JsString) -> bool {
        self.roots.host_modules.borrow().contains_key(specifier)
    }

    /// The module behind a namespace object, deferred or not (spec 10.4.6).
    pub fn module_namespace(&self, id: u64) -> Option<Handle<crate::module::SourceTextModule>> {
        self.roots.module_namespaces.borrow().get(&id).copied()
    }

    /// The module behind a deferred namespace object ([[Deferred]] = true).
    pub fn deferred_namespace(&self, id: u64) -> Option<Handle<crate::module::SourceTextModule>> {
        self.roots.deferred_namespaces.borrow().get(&id).copied()
    }

    /// The module behind an `%AbstractModuleSource%` instance.
    pub fn module_source(&self, id: u64) -> Option<Handle<crate::module::SourceTextModule>> {
        self.roots.module_sources.borrow().get(&id).copied()
    }

    /// A dynamic-import namespace resolver, when `id` names one.
    pub(crate) fn import_namespace_resolver(&self, id: u64) -> Option<(Value, Value)> {
        self.roots
            .import_namespace_resolvers
            .borrow()
            .get(&id)
            .copied()
    }

    /// The wait state of an `import.defer(...).then(...)` promise.
    pub(crate) fn deferred_module_wait(&self, id: u64) -> Option<crate::module::DeferredWait> {
        self.roots.deferred_module_waits.borrow().get(&id).cloned()
    }

    /// The module behind a DeferredModule `.then` method.
    pub(crate) fn deferred_module_then(
        &self,
        id: u64,
    ) -> Option<Handle<crate::module::SourceTextModule>> {
        self.roots.deferred_module_thens.borrow().get(&id).copied()
    }

    pub fn has_map(&self, id: u64) -> bool {
        self.roots.map_data.borrow().contains_key(&id)
    }

    pub fn has_set(&self, id: u64) -> bool {
        self.roots.set_data.borrow().contains_key(&id)
    }

    /// Whether `id` is a registered Map iterator. A predicate of its own
    /// because the record accessor hands out the record itself.
    pub fn has_map_iter(&self, id: u64) -> bool {
        self.roots.map_iter_data.borrow().contains_key(&id)
    }

    /// Whether `id` is a registered Set iterator.
    pub fn has_set_iter(&self, id: u64) -> bool {
        self.roots.set_iter_data.borrow().contains_key(&id)
    }

    /// The brand predicates for the per-object state tables: a value is a
    /// `DataView`, a Date, a RegExp … exactly when its object identity has a
    /// record in that table. The ones whose tables have not moved into
    /// [`AgentRoots`] read the field directly.
    pub fn has_data_view(&self, id: u64) -> bool {
        self.roots.dataview_data.borrow().contains_key(&id)
    }

    /// The state of a registered ArrayBuffer, or `None`. A guard into the
    /// table: keep it to the statement that reads it (see the field's note).
    pub fn buffer_state(
        &self,
        id: u64,
    ) -> Option<Ref<'_, crate::builtins::array_buffer::BufferState>> {
        Ref::filter_map(self.roots.buffer_data.borrow(), |table| table.get(&id)).ok()
    }

    /// The state of a registered DataView, or `None`.
    pub fn dataview_state(
        &self,
        id: u64,
    ) -> Option<Ref<'_, crate::builtins::dataview::DataViewState>> {
        Ref::filter_map(self.roots.dataview_data.borrow(), |table| table.get(&id)).ok()
    }

    pub fn has_buffer(&self, id: u64) -> bool {
        self.roots.buffer_data.borrow().contains_key(&id)
    }

    /// Insert a buffer's state. Under the table's own borrow, so callers must
    /// have let go of any `buffer_state` guard.
    pub(crate) fn register_buffer(
        &self,
        id: u64,
        state: crate::builtins::array_buffer::BufferState,
    ) {
        self.roots.buffer_data.borrow_mut().insert(id, state);
    }

    /// Mutate a buffer's state in place, if it is registered.
    pub(crate) fn with_buffer_state<T>(
        &self,
        id: u64,
        write: impl FnOnce(&mut crate::builtins::array_buffer::BufferState) -> T,
    ) -> Option<T> {
        let mut table = self.roots.buffer_data.borrow_mut();
        table.get_mut(&id).map(write)
    }

    pub(crate) fn register_dataview(
        &self,
        id: u64,
        state: crate::builtins::dataview::DataViewState,
    ) {
        self.roots.dataview_data.borrow_mut().insert(id, state);
    }

    pub fn has_date(&self, id: u64) -> bool {
        self.date_data.contains_key(&id)
    }

    pub fn has_regexp(&self, id: u64) -> bool {
        self.roots.regexp_data.borrow().contains_key(&id)
    }

    pub fn has_boolean(&self, id: u64) -> bool {
        self.boolean_data.contains_key(&id)
    }

    pub fn has_number(&self, id: u64) -> bool {
        self.number_data.contains_key(&id)
    }

    pub fn has_symbol(&self, id: u64) -> bool {
        self.roots.symbol_data.borrow().contains_key(&id)
    }

    pub fn has_bigint(&self, id: u64) -> bool {
        self.roots.bigint_data.borrow().contains_key(&id)
    }

    /// The [[SymbolData]] of a Symbol wrapper, when `id` is one.
    pub(crate) fn symbol(&self, id: u64) -> Option<crux::symbol::Symbol> {
        self.roots.symbol_data.borrow().get(&id).cloned()
    }

    pub(crate) fn register_symbol(&self, id: u64, symbol: crux::symbol::Symbol) {
        self.roots.symbol_data.borrow_mut().insert(id, symbol);
    }

    /// The [[BigIntData]] of a BigInt wrapper, when `id` is one.
    pub(crate) fn bigint(&self, id: u64) -> Option<crux::BigInt> {
        self.roots.bigint_data.borrow().get(&id).cloned()
    }

    pub(crate) fn register_bigint(&self, id: u64, value: crux::BigInt) {
        self.roots.bigint_data.borrow_mut().insert(id, value);
    }

    /// The RegExp state of `id`, when the object is a RegExp. A clone, because
    /// the compiled program is `Rc`-shared and every reader wants an owned
    /// copy anyway (see the table's note).
    pub fn regexp_state(&self, id: u64) -> Option<crate::builtins::regexp::RegExpState> {
        self.roots.regexp_data.borrow().get(&id).cloned()
    }

    pub(crate) fn register_regexp(&self, id: u64, state: crate::builtins::regexp::RegExpState) {
        self.roots.regexp_data.borrow_mut().insert(id, state);
    }

    /// The [[RawJSON]] text of a `JSON.rawJSON` object, when `id` is one.
    pub(crate) fn raw_json_text(&self, id: u64) -> Option<JsString> {
        self.roots.raw_json_data.borrow().get(&id).cloned()
    }

    pub(crate) fn has_raw_json(&self, id: u64) -> bool {
        self.roots.raw_json_data.borrow().contains_key(&id)
    }

    pub(crate) fn register_raw_json(&self, id: u64, text: JsString) {
        self.roots.raw_json_data.borrow_mut().insert(id, text);
    }

    pub fn has_weak_map(&self, id: u64) -> bool {
        self.roots.weak_map_data.borrow().contains_key(&id)
    }

    pub fn has_weak_set(&self, id: u64) -> bool {
        self.roots.weak_set_data.borrow().contains_key(&id)
    }

    pub(crate) fn register_map(&self, id: u64, entries: MapCollection) {
        self.roots
            .map_data
            .borrow_mut()
            .insert(id, Rc::new(RefCell::new(entries)));
    }

    pub(crate) fn register_set(&self, id: u64, entries: SetCollection) {
        self.roots
            .set_data
            .borrow_mut()
            .insert(id, Rc::new(RefCell::new(entries)));
    }

    pub(crate) fn register_weak_map(&self, id: u64, entries: Vec<MapEntry>) {
        self.roots
            .weak_map_data
            .borrow_mut()
            .insert(id, Rc::new(RefCell::new(entries)));
    }

    pub(crate) fn register_weak_set(&self, id: u64, entries: Vec<SetEntry>) {
        self.roots
            .weak_set_data
            .borrow_mut()
            .insert(id, Rc::new(RefCell::new(entries)));
    }

    pub(crate) fn register_map_iter(&self, id: u64, record: IterRecord) {
        self.roots
            .map_iter_data
            .borrow_mut()
            .insert(id, Rc::new(RefCell::new(record)));
    }

    pub(crate) fn register_set_iter(&self, id: u64, record: IterRecord) {
        self.roots
            .set_iter_data
            .borrow_mut()
            .insert(id, Rc::new(RefCell::new(record)));
    }

    /// The leaf-inline record for `id` (Cut 34): a direct-mapped cache over
    /// the `ecma_functions` map, so a hot leaf call skips the HashMap lookup.
    /// A miss resolves from the map and caches the entry (only for a
    /// leaf-inlineable function); function ids are never reused, so a hit
    /// needs no generation check.
    ///
    /// The entry is handed back by value: the cache lives behind the roots'
    /// `RefCell`, so no borrow of it may escape into the caller (which runs a
    /// body that allocates). The clone is one `Rc` bump plus a copied handle —
    /// the callers cloned the same fields out of the borrowed entry before.
    ///
    /// The hit path costs one `RefCell` borrow per leaf call. Splitting the
    /// fill path out (and marking it cold) measured *slower* — 11% against the
    /// baseline on a closure-call loop, versus 8% for this shape — so the
    /// branch stays in one function.
    pub(crate) fn leaf_lookup(&mut self, id: u64) -> Option<crate::ir::LeafEntry> {
        let index = id.wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize & (crate::ir::LEAF_CACHE - 1);
        let cached = {
            let cells = self.roots.leaf_cache.borrow();
            match cells[index].as_ref() {
                Some((cached_id, entry)) if *cached_id == id => Some(entry.clone()),
                _ => None,
            }
        };
        if let Some(entry) = cached {
            return Some(entry);
        }
        // The record guard stays inside this block: the cache write below is
        // an allocation-free store, but a guard is never worth extending.
        let (ir, strict, environment, construct_inline) = {
            let data = self.ecma_function(&id)?;
            if !data.leaf_inline {
                return None;
            }
            let ir = data.ir.clone().expect("leaf_inline implies a compiled ir");
            let environment = if ir.leaf_uses_env {
                Some(data.environment)
            } else {
                None
            };
            (ir, data.strict, environment, data.construct_inline)
        };
        let entry = crate::ir::LeafEntry {
            ir,
            strict,
            environment,
            // Cut 35 slice 15: the `Construct` step shares this cache,
            // so the construct-inline verdict rides along.
            construct_inline,
        };
        self.roots.leaf_cache.borrow_mut()[index] = Some((id, entry.clone()));
        Some(entry)
    }

    /// The direct-mapped builtin-call verdict for `id`: a probe that fills from
    /// the thread-local `BUILTIN_HANDLERS` and the agent's
    /// `builtin_dispatch_cache` on a miss, so a warm builtin call skips both
    /// hashes. Registrations complete at realm bootstrap and the dispatch memo
    /// only ever gains ids for `FunctionKind::Builtin` functions, so a fill is
    /// final. An id matching NEITHER source is not cached: it may be a builtin
    /// whose dispatch verdict `call_inner` has not memoized yet (the first
    /// general call fills it), so a later call must re-probe.
    pub(crate) fn builtin_call_lookup(
        &mut self,
        id: u64,
        is_native: bool,
    ) -> crate::function::BuiltinCall {
        let index = id.wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize
            & (crate::ir::BUILTIN_HANDLER_CELLS - 1);
        if let Some((cached_id, call)) = self.builtin_call_cells[index]
            && cached_id == id
        {
            return call;
        }
        // A host callback's re-entrancy is a property the embedding boundary
        // records once per function (`mark_host_reentrant`); an engine-defined
        // built-in is never marked. Folding the bit into the cached verdict
        // keeps the per-call cost a cell hit — the registry is read only when
        // the verdict is first computed.
        let call = if let Some(handler) = crate::function::builtin_handler(id) {
            crate::function::BuiltinCall::Handler {
                handler,
                reentrant: crate::function::host_reentrant(id),
            }
        } else if let Some(verdict) = self.builtin_dispatch_cache.get(&id) {
            if *verdict == 0 && is_native {
                crate::function::BuiltinCall::Native {
                    reentrant: crate::function::host_reentrant(id),
                }
            } else {
                crate::function::BuiltinCall::Other
            }
        } else {
            return crate::function::BuiltinCall::Other;
        };
        self.builtin_call_cells[index] = Some((id, call));
        call
    }

    /// The registered agent-dependent builtin CONSTRUCT handler for `id` (the
    /// construct-side mirror of `builtin_handler_lookup`): a direct-mapped
    /// probe that fills from the thread-local `CONSTRUCT_HANDLERS` on a miss.
    pub(crate) fn builtin_ctor_lookup(&mut self, id: u64) -> Option<crate::function::BuiltinCtor> {
        let index = id.wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize
            & (crate::ir::BUILTIN_HANDLER_CELLS - 1);
        let slot = &mut self.builtin_ctor_cells[index];
        if let Some((cached_id, ctor)) = slot
            && *cached_id == id
        {
            return Some(*ctor);
        }
        let ctor = crate::function::builtin_ctor(id)?;
        *slot = Some((id, ctor));
        Some(ctor)
    }

    /// spec 9.7.1 AgentSignifier.
    pub fn agent_signifier(&self) -> u64 {
        self.signifier
    }

    /// spec 9.7.2 AgentCanSuspend: false because [[CanBlock]] is false.
    pub fn agent_can_suspend(&self) -> bool {
        self.can_block
    }

    /// InitializeHostDefinedRealm (spec 9.3.4) and push the bootstrap
    /// execution context.
    pub fn initialize_host_defined_realm(&mut self) -> Result<Handle<Realm>, JsError> {
        self.initialize_host_defined_realm_with_global(None)
    }

    /// The same, for a realm whose global object is a host-defined exotic
    /// (`crux::host::HostOps`) — the seam V8 handler objects use.
    pub fn initialize_host_defined_realm_with_global(
        &mut self,
        global_ops: Option<std::rc::Rc<dyn crux::host::HostOps>>,
    ) -> Result<Handle<Realm>, JsError> {
        // Register as a live GC root before the bootstrap allocates the realm
        // boxes: an agent that holds a realm but has not yet entered a
        // `with_agent` window (the embedding created it and will evaluate it
        // later) would otherwise be invisible to a sibling's collection.
        live_agent_entered(self as *mut Agent as *mut ());
        let realm = initialize_host_defined_realm_with_global(self, global_ops)?;
        self.push_bootstrap_context(realm);
        Ok(realm)
    }

    /// Push the initial execution context created in
    /// InitializeHostDefinedRealm: Function and ScriptOrModule are null.
    pub fn push_bootstrap_context(&mut self, realm: Handle<Realm>) {
        let global_env = realm.global_env;
        self.roots.execution_context_stack.push(ExecutionContext {
            function: None,
            realm,
            script_or_module: None,
            lexical_environment: global_env,
            variable_environment: global_env,
            private_environment: None,
            source: None,
            annex_b_hoistable: Default::default(),
            position: None,
        });
    }

    /// Pop the context [`push_bootstrap_context`](Self::push_bootstrap_context)
    /// pushed. A host that brackets a script evaluation with a bootstrap
    /// context needs this to unwind it, and the stack is in `roots`.
    pub fn pop_bootstrap_context(&self) {
        self.roots.execution_context_stack.pop();
    }

    /// The current Realm Record (the Realm component of the running context).
    pub fn current_realm(&self) -> Result<Handle<Realm>, JsError> {
        // A running execution context wins, as its own context wins in V8's
        // `GetCurrentContext`; with none running — the drain runs a job with
        // no context of its own — the realm the job was enqueued for is what
        // remains.
        if let Some(context) = self.roots.execution_context_stack.last() {
            return Ok(context.realm);
        }
        if let Some(realm) = self.roots.entered_realm.get() {
            return Ok(realm);
        }
        Err(JsError::new(
            ErrorKind::ReferenceError,
            "No running execution context".into(),
        ))
    }

    /// The Promise Record of `id` (spec 27.2.1), when the object is a
    /// promise. Answers an owned handle so the caller does not hold the
    /// table's borrow across whatever it does with the record.
    pub fn promise_data(&self, id: u64) -> Option<Rc<RefCell<crate::promise::PromiseData>>> {
        self.roots.promises.borrow().get(&id).cloned()
    }

    /// Whether `id` names a promise object (it has a Promise Record).
    pub fn is_promise_object(&self, id: u64) -> bool {
        self.roots.promises.borrow().contains_key(&id)
    }

    /// Whether `id` names a generator object (the engine keys each one's
    /// suspended state by object identity).
    pub fn is_generator_object(&self, id: u64) -> bool {
        self.roots.generators.borrow().contains_key(&id)
    }

    /// The record of the resolving function `id`, when it is one
    /// (spec 27.2.1.3).
    pub fn promise_resolver_data(
        &self,
        id: u64,
    ) -> Option<Rc<RefCell<crate::promise::ResolverData>>> {
        self.roots.promise_resolvers.borrow().get(&id).cloned()
    }

    /// HostEnqueueGenericJob (spec 9.5.3): schedule a job without additional
    /// constraints such as priority.
    pub fn enqueue_generic_job(
        &mut self,
        realm: Option<Handle<Realm>>,
        closure: impl FnOnce(&mut Agent) -> Result<Value, JsError> + 'static,
    ) {
        self.roots
            .generic_jobs
            .borrow_mut()
            .push_back(Job::new(realm, closure));
    }

    /// HostEnqueuePromiseJob (spec 9.5.4): schedule a job at promise
    /// priority. Jobs run in the order their enqueues happened.
    pub fn enqueue_promise_job(
        &mut self,
        realm: Option<Handle<Realm>>,
        closure: impl FnOnce(&mut Agent) -> Result<Value, JsError> + 'static,
    ) {
        // A realm the host gave its own queue sends its jobs there, so the host
        // drains them itself. A queue the host released, or an id nothing
        // knows, falls back to the default queue rather than dropping the job.
        let queue_id = realm
            .and_then(|realm| realm.microtask_queue.get())
            .map(|id| id as usize);
        if let Some(queue_id) = queue_id
            && let Some(queue) = self
                .roots
                .microtask_queues
                .borrow_mut()
                .get_mut(queue_id)
                .and_then(Option::as_mut)
        {
            queue.jobs.push_back(Job::new(realm, closure));
            return;
        }
        self.roots
            .promise_jobs
            .borrow_mut()
            .push_back(Job::new(realm, closure));
    }

    /// Make a microtask queue the host drains itself (`v8::MicrotaskQueue::new`).
    ///
    /// The id answers what a context attaches with
    /// (`v8::ContextOptions::microtask_queue`) and what `PerformCheckpoint`
    /// drains. Ids are never reused, so a released one stays a no-op.
    pub fn new_microtask_queue(&mut self, policy: crate::api::MicrotasksPolicy) -> u32 {
        let mut queues = self.roots.microtask_queues.borrow_mut();
        let id = queues.len() as u32;
        queues.push(Some(MicrotaskQueue {
            policy,
            jobs: VecDeque::new(),
        }));
        id
    }

    /// Release a host's queue, which is what dropping its `v8::MicrotaskQueue`
    /// does: what the queue still holds is dropped, and its id goes quiet (its
    /// realm's later jobs then take the default queue).
    pub fn release_microtask_queue(&mut self, id: u32) {
        let mut queues = self.roots.microtask_queues.borrow_mut();
        if let Some(slot) = queues.get_mut(id as usize) {
            *slot = None;
        }
    }

    /// Run one queue's jobs to completion (`MicrotaskQueue::PerformCheckpoint`).
    ///
    /// Only that queue: the default queue and the other host queues are
    /// untouched, and work the jobs enqueue for the same realm comes back to
    /// this queue.
    pub fn run_microtask_queue(&mut self, id: u32) -> Result<(), JsError> {
        crux::function::with_agent(self as *mut Agent as *mut (), || {
            crux::heap::reset_allocation_budget();
            self.run_host_finalizers();
            loop {
                let Some(job) = self.next_microtask_queue_job(id) else {
                    break;
                };
                self.run_job(job)?;
            }
            self.maybe_collect();
            Ok(())
        })
    }

    /// The next job in queue `id`, if the host has not released it.
    fn next_microtask_queue_job(&mut self, id: u32) -> Option<Job> {
        self.roots
            .microtask_queues
            .borrow_mut()
            .get_mut(id as usize)
            .and_then(Option::as_mut)
            .and_then(|queue| queue.jobs.pop_front())
    }

    /// The next job in an `Auto` host queue, if any.
    ///
    /// The isolate's own drain runs those too, because that is what `Auto`
    /// means; an `Explicit` queue is only ever drained by its own
    /// `PerformCheckpoint`.
    fn next_auto_microtask_queue_job(&mut self) -> Option<Job> {
        let mut queues = self.roots.microtask_queues.borrow_mut();
        for slot in queues.iter_mut() {
            if let Some(queue) = slot
                && queue.policy == crate::api::MicrotasksPolicy::Auto
                && let Some(job) = queue.jobs.pop_front()
            {
                return Some(job);
            }
        }
        None
    }

    /// HostEnqueueTimeoutJob (spec 9.5.5): schedule a job to run after at
    /// least `milliseconds` milliseconds.
    pub fn enqueue_timeout_job(
        &mut self,
        realm: Option<Handle<Realm>>,
        milliseconds: u64,
        closure: impl FnOnce(&mut Agent) -> Result<Value, JsError> + 'static,
    ) {
        // The deadline is host-monotonic nanoseconds (`crate::time`), so
        // scheduling works on targets whose std clock is unsupported.
        let deadline =
            crate::time::monotonic_nanos().saturating_add(milliseconds.saturating_mul(1_000_000));
        self.roots
            .timeout_jobs
            .borrow_mut()
            .push_back((deadline, Job::new(realm, closure)));
    }

    /// RunJobs: drain the job queues — promise jobs first (FIFO), then due
    /// timeouts, then generic jobs — until nothing runnable remains.
    pub fn run_jobs(&mut self) -> Result<(), JsError> {
        crux::function::with_agent(self as *mut Agent as *mut (), || self.run_jobs_inner())
    }

    /// Run the host callbacks the collections since the last drain queued: the
    /// host finalizers (`crux::host`) and the weak-handle death callbacks
    /// (`crux::heap`) — the two things a collection has to tell a host, both
    /// deferred for the same reason.
    ///
    /// The sweep never runs host code itself — it runs mid-collection with the
    /// heap borrowed, and a callback may allocate or run JS — so this is the
    /// drain, and it happens outside a collection. A host that never asks
    /// is covered too: an outermost [`crate::api::Context`] entry drains here as
    /// well as doing so at the top of every job drain.
    pub(crate) fn run_host_finalizers(&mut self) {
        for (behaviour, object_id) in crux::host::take_pending_finalizers() {
            behaviour.finalize(object_id);
        }
        for callback in crux::heap::take_pending_weak_callbacks() {
            callback();
        }
    }

    fn run_jobs_inner(&mut self) -> Result<(), JsError> {
        // GC-5: the job drain is a fresh execution unit — the safe-point
        // allocation budget must not leak in from the previous script/job.
        crux::heap::reset_allocation_budget();
        // The post-collection drain, host bookkeeping first: a host finalizer
        // may itself enqueue a cleanup job or a promise job, and it is the
        // host's own release path rather than a JavaScript observer of one.
        self.run_host_finalizers();
        // GC-4: promote the FinalizationRegistry cleanup jobs enqueued by the
        // collector's compaction hook into the generic queue (the hook runs
        // with `&self` and cannot touch the queues directly).
        self.roots
            .generic_jobs
            .borrow_mut()
            .extend(self.roots.pending_cleanup_jobs.borrow_mut().drain(..));
        loop {
            let next = self.roots.promise_jobs.borrow_mut().pop_front();
            if let Some(job) = next {
                self.run_job(job)?;
                continue;
            }
            // Only read the deadline clock when a timer is actually queued
            // (on wasm32-unknown-unknown it is a host import).
            let due = {
                let jobs = self.roots.timeout_jobs.borrow();
                if jobs.is_empty() {
                    None
                } else {
                    let now = crate::time::monotonic_nanos();
                    jobs.iter().position(|(deadline, _)| *deadline <= now)
                }
            };
            if let Some(index) = due {
                let job = self.roots.timeout_jobs.borrow_mut().remove(index);
                if let Some((_, job)) = job {
                    self.run_job(job)?;
                    continue;
                }
            }
            let next = self.roots.generic_jobs.borrow_mut().pop_front();
            if let Some(job) = next {
                self.run_job(job)?;
                continue;
            }
            if let Some(job) = self.next_auto_microtask_queue_job() {
                self.run_job(job)?;
                continue;
            }
            break;
        }
        // GC-1 slice 3: the queue drain is a quiescent point (no job
        // closures hold values); a threshold-triggered collection here is
        // safe and bounds the cycle leaks.
        self.maybe_collect();
        Ok(())
    }

    fn run_job(&mut self, job: Job) -> Result<Value, JsError> {
        // GC-2: root the running closure's captures while it executes (see
        // `trace_roots`).
        let region = job.closure_region();
        RUNNING_JOB_REGION.with(|slot| *slot.borrow_mut() = Some(region));
        // The job runs in its own realm (spec 9.5.4): enter it for the closure,
        // so engine hooks that ask for the current realm answer one rather than
        // failing while a job is in flight.
        let previous = self.roots.entered_realm.get();
        self.roots.entered_realm.set(job.realm);
        let result = (job.closure)(self);
        self.roots.entered_realm.set(previous);
        RUNNING_JOB_REGION.with(|slot| *slot.borrow_mut() = None);
        // GC-4: the job ended — its KeepDuringJob set is no longer needed.
        self.roots.kept_during_job.borrow_mut().clear();
        result
    }

    pub fn job_queues_empty(&self) -> bool {
        self.roots.promise_jobs.borrow().is_empty()
            && self.roots.generic_jobs.borrow().is_empty()
            && self.roots.timeout_jobs.borrow().is_empty()
    }

    /// Nanoseconds until the earliest queued timeout is due (0 when a job is
    /// already due), or `None` when no timeout jobs are queued. Hosts use it
    /// to schedule the drain that lets engine timers progress.
    pub fn next_timeout_delay_ns(&self) -> Option<u64> {
        let now = crate::time::monotonic_nanos();
        self.roots
            .timeout_jobs
            .borrow()
            .iter()
            .map(|(deadline, _)| *deadline)
            .min()
            .map(|deadline| deadline.saturating_sub(now))
    }

    /// Parse and evaluate a Script (spec 16.1.4-16.1.6) in the current
    /// realm, returning the script's completion value.
    pub fn run_script(&mut self, source: &str) -> Result<Value, JsError> {
        self.run_script_named(source, None)
    }

    /// Like [`Agent::run_script`], for a script the host named: `name` is the
    /// script's resource name, which a host's dynamic-import callback is handed
    /// as the referrer of an `import()` written in this script.
    pub fn run_script_named(
        &mut self,
        source: &str,
        name: Option<JsString>,
    ) -> Result<Value, JsError> {
        self.run_script_mode(source, false, name)
    }

    /// Like [`Agent::run_script`], parsing with the JSX extension enabled:
    /// JSX elements desugar to `rlx.h(...)` calls at parse time.
    pub fn run_script_jsx(&mut self, source: &str) -> Result<Value, JsError> {
        self.run_script_mode(source, true, None)
    }

    fn run_script_mode(
        &mut self,
        source: &str,
        jsx: bool,
        name: Option<JsString>,
    ) -> Result<Value, JsError> {
        crux::function::with_agent(self as *mut Agent as *mut (), || {
            // GC-5: a fresh script is a fresh execution unit — the safe-point
            // allocation budget must not leak in from the previous script.
            crux::heap::reset_allocation_budget();
            // Release the records of closures a previous script's boundary
            // collection swept (the death queue is drained at every run
            // entry and collection trigger).
            self.reap_dead_functions();
            self.reap_dead_wrappers();
            self.reap_dead_promises();
            let realm = self.current_realm()?;
            let script = if jsx {
                crate::script::parse_script_jsx(source, realm, name)?
            } else {
                crate::script::parse_script(source, realm, name)?
            };
            let result = crate::script::script_evaluation(self, &script);
            // GC-1 slice 3: a script boundary with no pending jobs is a
            // quiescent point — every live value is reachable from the
            // traced agent roots or the native stack, so a threshold-
            // triggered collection there cannot free a box a queued job
            // closure (opaque to tracing) still captures.
            if self.job_queues_empty() {
                self.maybe_collect();
            }
            result
        })
    }

    /// GC-1 slice 3: gather the precise roots and mark-sweep with the
    /// conservative native-stack scan. Must run at a quiescent point (no
    /// job closures in flight, no active RefCell borrows on the traced
    /// tables). Takes `&self` so the `--gc-stress` collector can run it from
    /// inside code that already holds `&mut Agent` without aliasing it.
    pub fn collect_garbage(&self) {
        self.collect_garbage_with(None);
    }

    /// [`Agent::collect_garbage`] with an extra root: the fresh box of the
    /// allocation that triggered a `--gc-stress` collection (GC-2).
    pub fn collect_garbage_with(&self, extra: Option<GcAny>) {
        self.collect_with(extra, false);
    }

    /// A3: a minor (young-generation) collection with an extra root. Only the
    /// boxes allocated since the last collection are at risk, so its cost is
    /// paced by the young cohort rather than the live set.
    pub fn collect_minor_garbage_with(&self, extra: Option<GcAny>) {
        self.collect_with(extra, true);
    }

    /// Register an observer of this agent's collections
    /// (`v8::Isolate::AddGCPrologueCallback` and its epilogue sibling, whose
    /// engine side this is).
    ///
    /// The observer runs around every collection this agent drives, prologue
    /// before the mark and epilogue after the sweep, with the generation. A
    /// host that measures a pause needs both ends, since a sweep's cost is the
    /// half a prologue-only hook cannot see.
    pub fn add_collection_observer(&self, observer: Box<dyn crux::heap::GcObserver>) {
        self.collection_observers.borrow_mut().push(observer);
    }

    /// The shared collection driver: the roots, the weak-compaction hook, and
    /// the abort conditions are identical for both generations; `minor` selects
    /// the young-only collector and its bookkeeping.
    fn collect_with(&self, extra: Option<GcAny>, minor: bool) {
        let generation = if minor {
            crux::heap::GcGeneration::Minor
        } else {
            crux::heap::GcGeneration::Major
        };
        // Taken out for the duration: an observer runs host code, and this list
        // must not be borrowed while that runs. A collection an observer
        // triggers therefore runs none of its own (see the field's note).
        let observers = std::mem::take(&mut *self.collection_observers.borrow_mut());
        for observer in &observers {
            observer.prologue(generation);
        }
        // GC-2: a native build in progress (the class element build holds
        // `build_roots`) — abort the sweep (retain everything) so its local
        // buffers cannot be swept.
        if self.build_roots.get() {
            crux::heap::note_aborted_trace();
        }
        let mut roots: Vec<GcAny> = Vec::new();
        if let Some(extra) = extra {
            roots.push(extra);
        }
        // Every live agent on the thread is a precise root: the crux heap is
        // shared, but each agent's state is reachable only from its own
        // Rust-heap tables, so a nested or sibling agent's boxes would be
        // swept otherwise. `self` is traced below.
        let self_roots = Rc::as_ptr(&self.roots);
        let live = live_agent_roots();
        for &agent_roots in &live {
            if agent_roots == self_roots {
                continue;
            }
            // SAFETY: a registered roots allocation is alive — registration is
            // removed only in `Agent::drop`, which cannot run during a
            // collection on this thread, and the allocation is owned by the
            // agent.
            unsafe {
                (&*agent_roots).trace(&mut |any| roots.push(any));
                (&*agent_roots).trace_active_run_state(&mut |any| roots.push(any));
            }
        }
        self.roots.trace(&mut |any| roots.push(any));
        self.roots
            .trace_active_run_state(&mut |any| roots.push(any));
        // GC-3/GC-4: the compaction hook runs between the mark and the sweep
        // (the would-be-swept boxes are still allocated), dropping dead weak
        // entries and capturing FinalizationRegistry held values into
        // cleanup jobs. The `precise` flag requests a scan-free mark for the
        // compaction's dead set: stale stack words must not keep a WeakRef
        // or FinalizationRegistry target alive (the sweep still uses the
        // conservative mark — the scan stays the safety net for Rust-held
        // handles).
        let has_weak = self.roots.has_weak_structures()
            || live.iter().any(|&agent_roots| {
                agent_roots != self_roots
                    // SAFETY: as above.
                    && unsafe { (&*agent_roots).has_weak_structures() }
            });
        // Cut 35 slice 31: the compaction hook (dead-set HashSet build +
        // weak-table walks) runs only when a weak structure actually exists
        // — the benchmark's collections have none, and the SipHash HashSet
        // of every dead address was measurable per collection.
        let compact: &mut crux::heap::CompactHook = if has_weak {
            &mut |dead, retain| {
                self.roots.compact_weak_tables(dead, retain);
                for &agent_roots in &live {
                    if agent_roots != self_roots {
                        // SAFETY: as above.
                        unsafe { (&*agent_roots).compact_weak_tables(dead, retain) };
                    }
                }
            }
        } else {
            &mut |_, _| {}
        };
        // Install the agent's own heap for the collection, when it has one: a
        // collection may run with no isolate entered (see the field).
        let installed = self.heap_state.get();
        let _installed = if installed.is_null() {
            None
        } else {
            crux::heap::enter(installed);
            Some(InstalledHeap)
        };
        let mut swept_addrs = crux::heap::with_heap_mut(|heap| {
            if minor {
                heap.collect_minor_with_stack(&roots, has_weak, compact)
            } else {
                heap.collect_with_stack_compacting(&roots, has_weak, compact)
            }
        });
        let swept = swept_addrs.len();
        // Drop Array Iterator entries whose iterator box the sweep reclaimed.
        // The sweep is the conservative mark's result, so an iterator held only
        // in a Rust stack local (the scan's job) is never pruned. The iterated
        // array was marked live through the map this cycle and is reclaimed on
        // the next one. The sweep's address list is arena order, not sorted;
        // sort it only when an iterator table exists (the common no-iterator
        // collection skips the work).
        let has_iterators = self
            .roots
            .array_iter_data
            .try_borrow()
            .is_ok_and(|map| !map.is_empty())
            || live.iter().any(|&agent_roots| {
                agent_roots != self_roots
                    // SAFETY: as above.
                    && unsafe { (&*agent_roots).array_iter_data.try_borrow().is_ok_and(|map| !map.is_empty()) }
            });
        // Every promise ever constructed has an entry in `promise_addrs`, and
        // `promises` (traced) retains each settled promise's result and
        // reactions until the id is reaped.
        let has_promise_addrs = self
            .roots
            .promise_addrs
            .try_borrow()
            .is_ok_and(|map| !map.is_empty())
            || live.iter().any(|&agent_roots| {
                agent_roots != self_roots
                    // SAFETY: as above.
                    && unsafe { (&*agent_roots).promise_addrs.try_borrow().is_ok_and(|map| !map.is_empty()) }
            });
        let has_generators = self
            .roots
            .generators
            .try_borrow()
            .is_ok_and(|map| !map.is_empty())
            || self
                .roots
                .async_generators
                .try_borrow()
                .is_ok_and(|map| !map.is_empty())
            || live.iter().any(|&agent_roots| {
                agent_roots != self_roots
                    // SAFETY: as above.
                    && unsafe {
                        (&*agent_roots)
                            .generators
                            .try_borrow()
                            .is_ok_and(|map| !map.is_empty())
                            || (&*agent_roots)
                                .async_generators
                                .try_borrow()
                                .is_ok_and(|map| !map.is_empty())
                    }
            });
        if has_iterators || has_promise_addrs || has_generators {
            swept_addrs.sort_unstable();
        }
        if has_iterators {
            self.roots.reap_dead_iterators(&swept_addrs);
            for &agent_roots in &live {
                if agent_roots != self_roots {
                    // SAFETY: as above.
                    unsafe { (&*agent_roots).reap_dead_iterators(&swept_addrs) };
                }
            }
        }
        if has_promise_addrs {
            self.roots.reap_promise_addrs(&swept_addrs);
            for &agent_roots in &live {
                if agent_roots != self_roots {
                    // SAFETY: as above.
                    unsafe { (&*agent_roots).reap_promise_addrs(&swept_addrs) };
                }
            }
        }
        if has_generators {
            self.roots.reap_dead_generators(&swept_addrs);
            for &agent_roots in &live {
                if agent_roots != self_roots {
                    // SAFETY: as above.
                    unsafe { (&*agent_roots).reap_dead_generators(&swept_addrs) };
                }
            }
        }
        for observer in &observers {
            observer.epilogue(generation);
        }
        *self.collection_observers.borrow_mut() = observers;
        if minor {
            // A minor reclassifies only its own cohort, so the major's growth
            // baseline (the post-major live count) is deliberately untouched.
            crux::heap::note_minor_collection(swept);
        } else {
            self.last_collected_live
                .set(crux::heap::with_heap(|heap| heap.live_count()));
            crux::heap::note_collection(swept);
        }
    }

    /// A3 `--gc-verify`: after every minor collection, run a full precise mark
    /// and assert that nothing it would sweep is reachable. Implied by
    /// `--gc-stress`; exposed on its own for a normal-mode audit.
    ///
    /// It also turns the A2 write-barrier verifier on: a minor's correctness
    /// rests on the remembered set being complete, and `--gc-stress` cannot
    /// audit that in normal-mode operation (its per-allocation collection
    /// promotes every fresh box, so a store into an old container rarely has a
    /// young value to record).
    pub fn set_gc_verify(&self, enabled: bool) {
        crux::heap::set_verify_minor(enabled);
        if enabled {
            crux::heap::set_verify_barrier(true);
        }
    }

    /// The safe-point collection trigger (A3 splits it by generation, A5 makes
    /// the levels independent): a minor collection is paced by the young cohort,
    /// a major by the old generation having grown past twice the post-major live
    /// count. Each level carries its own backoff, so a major that swept nothing
    /// suppresses only majors and a minor that reclaimed nothing suppresses only
    /// minors — a growing-live-set loop stops re-marking without stopping the
    /// other level's reclamation. Under `--gc-stress` both run at every safe
    /// point, so one sweep audits the barrier's old->young edges and the root
    /// set together.
    pub(crate) fn maybe_collect(&mut self) {
        // Release the records of closures the collector swept since the
        // last trigger (their boxes died at the sweep; the records' traced
        // `environment` roots would otherwise pin the captured chains
        // forever). The ids come from the crux-side death queue the sweep's
        // `drop_in_place` filled; ids this agent never registered (builtin
        // or bound functions) are filtered out.
        self.reap_dead_functions();
        self.reap_dead_wrappers();
        // A `StressSuppress` window — and a running compile — holds GC handles
        // in local buffers the stack scan cannot see, which is the whole reason
        // the window suppresses the per-allocation collector. A safe-point
        // collection is just as unsafe inside it: a handler held only in a
        // builtin's local `Vec` would be swept out from under the call. So the
        // window suppresses this trigger too.
        let suppressed = crate::ir::is_compiling();
        let live = crux::heap::with_heap(|heap| heap.live_count());
        let young = crux::heap::with_heap(|heap| heap.young_count());
        let threshold = self.last_collected_live.get().max(1024).saturating_mul(2);
        // The young cohort is the minor's to collect. Pace it by its size, and
        // also collect it when the heap has grown past the major threshold on
        // young alone: a total-live comparison fires a full major there, but the
        // growth is young, so a minor reclaims it for a fraction of the cost.
        // This is the shape a churn loop hits — the young reaches the major
        // threshold (about half the post-major live) well before the nursery
        // threshold, so without this the major would starve the minor.
        let young_dominated = live > threshold && young > 0;
        if !suppressed
            && (self.gc_stress.get()
                || (!crux::heap::minor_disabled()
                    && (young >= self.nursery_threshold.get() || young_dominated)))
        {
            self.clear_derived_caches();
            self.collect_minor_garbage_with(None);
        }
        // A major paces the *old generation's* growth. `live_count` carries the
        // young cohort still awaiting its minor, so the comparison subtracts it
        // and only promoted growth counts.
        let live = crux::heap::with_heap(|heap| heap.live_count());
        let young = crux::heap::with_heap(|heap| heap.young_count());
        let old_gen_live = live.saturating_sub(young);
        if !suppressed
            && (self.gc_stress.get() || (!crux::heap::major_disabled() && old_gen_live > threshold))
        {
            self.clear_derived_caches();
            self.collect_garbage();
        }
        // Drain the promise ids the sweep reclaimed. The entry's traced
        // `PromiseData` holds the settled result and its reactions, which is
        // the bulk of a long-lived promise's retained graph.
        self.reap_dead_promises();
        // After the collections, so a host's heap-limit callback is not fired
        // for a heap a collection just brought back under the limit.
        self.check_heap_limit();
    }

    /// Drop the caches whose contents a live receiver already reaches before a
    /// collection walks the roots.
    ///
    /// Each is keyed by a receiver the hit path validates by (id, generation),
    /// and what it holds is derivable from that receiver: an own property's
    /// value (the member-read and array-element cells) or a base's enumeration
    /// (the for-in enum cache). Rooting the contents is therefore redundant —
    /// and it retains what the receiver no longer reaches. That is what a
    /// create-heavy loop paid for: every member-read cell held a per-iteration
    /// closure and every for-in enum cell a dead per-iteration base with its
    /// key strings, so a minor promoted them instead of sweeping them, and the
    /// loop accumulated garbage until the next major (measured 2026-09-20 on
    /// the captured-closure shape and the for-in head-let row: 53 and 260
    /// promoted boxes per minor, and the per-body cost grew with the iteration
    /// count where the interpreter's stayed flat — 1.15x at N=10k, 2.1x at
    /// N=300k).
    ///
    /// Clearing is safe at this point because every handle in a cell has been
    /// a root since the previous clear, so no sweep could have freed it, and
    /// because a for-in loop in flight owns its own copy of the key list (the
    /// Vm's `for_in_stack`, traced as an active run). The reads that follow
    /// re-warm the caches through the full Get/enumerate paths, and the
    /// warm-store front re-records the value cells, so nothing observes the
    /// difference beyond one cold read per site per collection.
    fn clear_derived_caches(&mut self) {
        for cell in self.roots.member_value_cells.iter() {
            cell.set(crate::ir::MemberValueCell::empty());
        }
        for cell in self.roots.array_element_value_cells.iter() {
            cell.set(None);
        }
        for cell in self.roots.for_in_cells.borrow_mut().iter_mut() {
            *cell = None;
        }
    }

    /// Drop the per-function records of closures whose boxes a sweep
    /// reclaimed (see [`Function::drop`] and the dead-function queue). Each
    /// record holds the closure's [[Environment]] — the per-iteration env a
    /// loop-created closure captures — so releasing it lets the collector
    /// reclaim the whole captured chain. The matching leaf-cache cell is
    /// cleared too: it also holds the closure env and would keep it
    /// reachable.
    fn reap_dead_functions(&mut self) {
        let dead = crux::function::take_dead_function_ids();
        if dead.is_empty() {
            return;
        }
        for id in dead {
            // A resolver record is keyed by the function's id and outlives the
            // function by one collection; release it here so the table does
            // not accumulate stale records.
            self.roots.promise_resolvers.borrow_mut().remove(&id);
            if self.remove_ecma_function(&id).is_none() {
                continue;
            }
            let index =
                id.wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize & (crate::ir::LEAF_CACHE - 1);
            let mut cells = self.roots.leaf_cache.borrow_mut();
            if let Some((cached_id, _)) = &cells[index]
                && *cached_id == id
            {
                cells[index] = None;
            }
        }
    }

    /// Drop the per-object records of primitive-wrapper boxes a sweep
    /// reclaimed (see [`crux::object::JsObject`]'s `Drop` and the dead-id
    /// queue). The `number_data`/`boolean_data`/`bigint_data` tables answer
    /// the "is this a wrapper / what does it wrap" checks (valueOf, JSON
    /// stringify) and hold no handle to the object itself, so a dead
    /// wrapper's entry is pure retained memory — ids are never reused, so
    /// removing it is always safe.
    fn reap_dead_wrappers(&mut self) {
        let dead = crux::object::take_dead_wrapper_ids();
        if dead.is_empty() {
            return;
        }
        for id in dead {
            self.number_data.remove(&id);
            self.boolean_data.remove(&id);
            self.roots.bigint_data.borrow_mut().remove(&id);
        }
    }

    fn reap_dead_promises(&mut self) {
        let ids = std::mem::take(&mut *self.roots.pending_dead_promises.borrow_mut());
        for id in ids {
            self.roots.promises.borrow_mut().remove(&id);
        }
    }

    /// Toggle the `--gc-stress` mode: collect at every safe point instead of
    /// only on heap growth. Settable through `&Agent` (the cell).
    ///
    /// It also turns on the A2 write-barrier verifier: the stress net's whole
    /// job is to catch a missed root, and a missed old->young edge is the
    /// same class of bug (a young box swept out from under a live container),
    /// so the two audits run together.
    pub fn set_gc_stress(&self, enabled: bool) {
        self.gc_stress.set(enabled);
        // The profile default is on in debug builds; stress forces it on
        // regardless (see `crux::heap::set_verify_barrier`).
        crux::heap::set_verify_barrier(enabled || cfg!(debug_assertions));
        // A3: `--gc-stress` also forces the minor collection's self-check on, so
        // one stress sweep proves the barrier's old->young edges and the minor's
        // generation rule at the same time.
        crux::heap::set_verify_minor(enabled);
        if enabled {
            // GC-2: collect after *every* allocation. The collector finds
            // the current agent through the with_agent TLS window and roots
            // the fresh box; outside an agent window (realm bootstrap) it
            // is a no-op.
            crux::heap::enable_stress_collector(Box::new(|fresh| {
                if let Ok(agent) = crate::context::current_agent()
                    && !crate::ir::is_compiling()
                {
                    agent.collect_garbage_with(Some(fresh));
                }
            }));
        } else {
            crux::heap::disable_stress_collector();
        }
    }

    /// A5 `--nursery-threshold N`: the young-cohort size that paces a minor
    /// collection at a safe point. The tuning knob the cut exists for.
    pub fn set_nursery_threshold(&self, threshold: usize) {
        self.nursery_threshold.set(threshold.max(1));
    }

    /// A5 `--nursery-stress`: run a minor collection at **every safe point**
    /// instead of every `--nursery-threshold` allocations, leaving the major on
    /// its normal trigger.
    ///
    /// The plan's wording was "a minor on every allocation"; the safe point is
    /// the useful cadence, because a per-allocation collector runs *before* the
    /// store that would create the edge it exists to find: the fresh box is the
    /// collection's own root, so it is promoted by its own allocation and the
    /// store is old->old. Landing after a run of stores, the safe point is where
    /// old->young edges actually get collected.
    ///
    /// It turns both verifiers on: a barrier gap is the failure this mode is
    /// for.
    pub fn set_nursery_stress(&self, enabled: bool) {
        self.nursery_threshold.set(if enabled {
            1
        } else {
            DEFAULT_NURSERY_THRESHOLD
        });
        if enabled {
            crux::heap::set_verify_barrier(true);
            crux::heap::set_verify_minor(true);
        }
    }
}

impl Default for Agent {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether atomic operations on values of `bytes` bytes are lock-free
/// ([[IsLockFree1/2/8]]). Implementation-defined per spec 9.7; 4-byte
/// operations are always lock-free (there is no [[IsLockFree4]]).
fn is_lock_free_for_size(bytes: usize) -> bool {
    matches!(bytes, 1 | 2) || (bytes == 8 && cfg!(target_pointer_width = "64"))
}

/// A helper used by tests and the CLI: create an agent, bootstrap its
/// realm, evaluate `source`, then drain the job queues (the bootstrap
/// pipeline's RunJobs step).
pub fn evaluate(source: &str) -> Result<Value, JsError> {
    let mut agent = Agent::new();
    agent.initialize_host_defined_realm()?;
    let value = agent.run_script(source)?;
    agent.run_jobs()?;
    Ok(value)
}

/// Bounds on the compiled-regexp cache. A live `RegExp` object already keeps
/// its own program alive; the cache only extends it after the object dies, so
/// capping the entries (and the source length, since the program's size tracks
/// the source) keeps a script that builds many distinct patterns from pinning
/// them.
const MAX_CACHED_REGEXPS: usize = 64;
const MAX_CACHED_PATTERN_LEN: usize = 1024;

impl Agent {
    /// The compiled program for `pattern` with `flags`, reusing one compiled
    /// earlier for the same source and flags. `regexp::Regex` is immutable
    /// after construction, so the fresh object every literal evaluation creates
    /// can share it.
    pub(crate) fn compile_regexp(
        &mut self,
        pattern: &[u16],
        flags: regexp::Flags,
    ) -> Result<std::rc::Rc<regexp::Regex>, JsError> {
        if let Some(entries) = self.compiled_regexps.get(pattern)
            && let Some((_, compiled)) = entries.iter().find(|(cached, _)| *cached == flags)
        {
            return Ok(compiled.clone());
        }
        let compiled = std::rc::Rc::new(
            regexp::compile(pattern, flags)
                .map_err(|error| JsError::new(ErrorKind::SyntaxError, error.message.clone()))?,
        );
        let already_known = self.compiled_regexps.contains_key(pattern);
        if pattern.len() <= MAX_CACHED_PATTERN_LEN
            && (already_known || self.compiled_regexps.len() < MAX_CACHED_REGEXPS)
        {
            self.compiled_regexps
                .entry(pattern.to_vec())
                .or_default()
                .push((flags, compiled.clone()));
        }
        Ok(compiled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_regexps_are_shared_by_source_and_flags() {
        let mut agent = Agent::new();
        let none = regexp::Flags::none();
        let first = agent.compile_regexp(&[b'a' as u16], none).unwrap();
        let second = agent.compile_regexp(&[b'a' as u16], none).unwrap();
        assert!(std::rc::Rc::ptr_eq(&first, &second));

        let mut global = none;
        global.g = true;
        let global = agent.compile_regexp(&[b'a' as u16], global).unwrap();
        assert!(!std::rc::Rc::ptr_eq(&first, &global));

        let other = agent.compile_regexp(&[b'b' as u16], none).unwrap();
        assert!(!std::rc::Rc::ptr_eq(&first, &other));
    }

    #[test]
    fn compiled_regexp_cache_is_bounded() {
        let mut agent = Agent::new();
        let none = regexp::Flags::none();
        let first_source = [b'a' as u16];
        let first = agent.compile_regexp(&first_source, none).unwrap();
        for index in 0..MAX_CACHED_REGEXPS {
            let pattern = vec![b'b' as u16; index + 1];
            agent.compile_regexp(&pattern, none).unwrap();
        }
        // The source cap is full, so a new source is compiled but not retained.
        let extra = [b'z' as u16, b'z' as u16];
        let extra_first = agent.compile_regexp(&extra, none).unwrap();
        let extra_second = agent.compile_regexp(&extra, none).unwrap();
        assert!(!std::rc::Rc::ptr_eq(&extra_first, &extra_second));
        // A source cached before the cap still hits.
        let first_again = agent.compile_regexp(&first_source, none).unwrap();
        assert!(std::rc::Rc::ptr_eq(&first, &first_again));
    }

    #[test]
    fn long_regexp_sources_are_not_cached() {
        let mut agent = Agent::new();
        let none = regexp::Flags::none();
        let pattern = vec![b'a' as u16; MAX_CACHED_PATTERN_LEN + 1];
        let first = agent.compile_regexp(&pattern, none).unwrap();
        let second = agent.compile_regexp(&pattern, none).unwrap();
        assert!(!std::rc::Rc::ptr_eq(&first, &second));
    }

    #[test]
    fn agent_record_fields_are_set() {
        let agent = Agent::new();
        assert!(!agent.can_block);
        assert!(!agent.agent_can_suspend());
        assert_eq!(agent.little_endian, cfg!(target_endian = "little"));
        assert_eq!(agent.is_lock_free.len(), 3);
    }

    #[test]
    fn signifiers_are_unique() {
        let a = Agent::new();
        let b = Agent::new();
        assert_ne!(a.agent_signifier(), b.agent_signifier());
    }

    #[test]
    fn bootstrap_context_sets_global_environments() {
        let mut agent = Agent::new();
        let realm = agent.initialize_host_defined_realm().unwrap();
        let context = agent.running_context().unwrap();
        assert!(context.function.is_none());
        assert!(context.script_or_module.is_none());
        assert!(context.private_environment.is_none());
        assert_eq!(context.realm.global_object, realm.global_object);
    }

    #[test]
    fn running_context_requires_a_bootstrap_context() {
        let agent = Agent::new();
        assert!(agent.running_context().is_err());
        assert!(agent.current_realm().is_err());
    }

    #[test]
    fn an_observer_brackets_each_collection_with_its_generation() {
        use crux::heap::{GcGeneration, GcObserver};
        use std::cell::RefCell;
        use std::rc::Rc;

        struct Watch(Rc<RefCell<Vec<(&'static str, GcGeneration)>>>);

        impl GcObserver for Watch {
            fn prologue(&self, generation: GcGeneration) {
                self.0.borrow_mut().push(("prologue", generation));
            }

            fn epilogue(&self, generation: GcGeneration) {
                self.0.borrow_mut().push(("epilogue", generation));
            }
        }

        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        agent.add_collection_observer(Box::new(Watch(Rc::clone(&seen))));

        agent.collect_garbage();
        agent.collect_minor_garbage_with(None);

        assert_eq!(
            *seen.borrow(),
            vec![
                ("prologue", GcGeneration::Major),
                ("epilogue", GcGeneration::Major),
                ("prologue", GcGeneration::Minor),
                ("epilogue", GcGeneration::Minor),
            ],
            "each collection is bracketed, and the two generations are told apart"
        );
    }

    #[test]
    fn timeout_jobs_run_when_due() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut agent = Agent::new();
        let realm = agent.initialize_host_defined_realm().unwrap();
        let order: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        agent.enqueue_timeout_job(Some(realm), 0, {
            let order = order.clone();
            move |_| {
                order.borrow_mut().push("timed");
                Ok(Value::Undefined)
            }
        });
        agent.run_jobs().unwrap();
        assert_eq!(*order.borrow(), vec!["timed"]);
    }

    #[test]
    fn a_minor_keeps_a_young_element_of_an_old_array() {
        // A5.1: a growing old array is exactly the shape the dirty low-water
        // mark exists for — the minor scans from the first appended element, and
        // must still find the young element appended since the last collection.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("globalThis.a = []; for (var i = 0; i < 300; i++) { a.push({ i: i }); }")
            .unwrap();
        // Promote the array and its 300 elements.
        agent.collect_garbage();
        // A young element appended to the now-old array is reachable only
        // through it, so only the remembered set's dirty range can find it.
        agent
            .run_script("var fresh = { tag: 'kept' }; a.push(fresh);")
            .unwrap();
        agent.collect_minor_garbage_with(None);
        assert_eq!(
            agent
                .run_script("a[a.length - 1].tag === 'kept' && a[0].i === 0 && a.length === 301")
                .unwrap(),
            Value::Boolean(true),
            "the young element survived the minor and the old elements are intact"
        );
    }

    #[test]
    fn sibling_agent_state_survives_other_agents_gc_stress() {
        // Two agents share the thread-local crux heap, but a collection roots
        // only the collecting agent's tables. Box `a` so the conservative
        // stack scan cannot save its state from its inline fields, then run
        // `b` under `--gc-stress` (a collection after every allocation):
        // before the fix `a`'s world was swept out from under it (release AV;
        // the debug arena masks it).
        let mut a = Box::new(Agent::new());
        a.initialize_host_defined_realm().unwrap();
        a.run_script(
            "globalThis.keep = function f(n) { return { tag: 'kept', nested: { v: n } }; }; globalThis.marker = 'alive';",
        )
        .unwrap();
        let mut b = Agent::new();
        b.initialize_host_defined_realm().unwrap();
        b.set_gc_stress(true);
        b.run_script(
            "var acc = []; for (var i = 0; i < 4000; i++) { acc.push({ i: i }); } globalThis.acc = acc.length;",
        )
        .unwrap();
        b.set_gc_stress(false);
        let ok = a
            .run_script("globalThis.marker === 'alive' && globalThis.keep(7).nested.v === 7")
            .unwrap();
        assert_eq!(ok.as_boolean(), Some(true));
    }

    #[test]
    fn realm_initialization_registers_the_agent_as_a_live_root() {
        // Realm bootstrap happens outside any `with_agent` window, so the
        // agent must be registered at initialization (not just at its first
        // run) — otherwise a sibling's collection could sweep its realm.
        let fresh = Agent::new();
        let fresh_ptr = &fresh as *const Agent;
        assert!(!is_live_agent(fresh_ptr));
        let ptr = {
            let mut agent = Agent::new();
            agent.initialize_host_defined_realm().unwrap();
            let ptr = &agent as *const Agent;
            assert!(is_live_agent(ptr));
            ptr
        };
        assert!(!is_live_agent(ptr), "drop must unregister");
    }
}
