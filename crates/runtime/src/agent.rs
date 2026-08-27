//! Agents (spec 9.7) and the surrounding-agent operations.
//!
//! The agent owns the execution context stack and the job queues; its
//! record fields ([[]] names below) are the Agent Record fields of the
//! spec 9.7 table. Single-threaded: [[CanBlock]] is false, so
//! AgentCanSuspend() is false.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

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
use crate::realm::{Realm, initialize_host_defined_realm};

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

/// The cached "the Array-iteration infrastructure is stock" verdict (Cut
/// 24): %Array.prototype%'s own @@iterator is the intrinsic,
/// %ArrayIteratorPrototype% has the stock `next`, and no `return` on the
/// AIP → %Object.prototype% chain. The handles are kept so a probe can
/// re-read the generations; any mutation bumps one and re-resolves the
/// full check.
pub(crate) struct ForOfFastVerdict {
    pub array_proto: (u64, u32),
    pub aip: (u64, u32),
    pub aip_handle: Handle<crux::object::JsObject>,
    pub object_proto: (u64, u32),
    pub object_proto_handle: Handle<crux::object::JsObject>,
}

impl Trace for ForOfFastVerdict {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.aip_handle.trace(visit);
        self.object_proto_handle.trace(visit);
    }
}

pub struct Agent {
    pub execution_context_stack: Vec<ExecutionContext>,
    /// The IC caches shared by every Vm run in this agent (the global-var
    /// cells and the P3 member/element cells). They lived on the Vm before,
    /// so every function call and script evaluation started cold and
    /// re-zeroed ~900 bytes of cache; they are re-validated on every access
    /// (against the current realm's global object and each object's property
    /// vector), so sharing them across Vms — and across realms — is exact
    /// and warms a nested call to its caller's shapes.
    pub(crate) global_cells: [Option<(crux::AtomId, usize)>; crate::ir::GLOBAL_CELLS],
    /// The global call-site leaf cache (Cut 35 slice 12): name → the
    /// resolved leaf entry for a stable global callee, valid while the
    /// global object's identity and generation are unchanged. Boxed so the
    /// Agent's hot-field cache footprint stays small (the Cut 27 lesson).
    pub(crate) global_leaf_cells: Box<[Option<crate::ir::GlobalLeafCell>; crate::ir::GLOBAL_CELLS]>,
    /// The slot-callee leaf cache (Cut 35 slice 12): frame-slot index → the
    /// resolved leaf entry for the closure held there, validated by the
    /// callee's heap payload (the held `callee` keeps it alive). Boxed per
    /// the Cut 27 lesson.
    pub(crate) slot_leaf_cells: Box<[Option<crate::ir::SlotLeafCell>; crate::ir::SLOT_LEAF_CELLS]>,
    pub(crate) member_cells: [Option<(u64, crux::AtomId, usize)>; crate::ir::MEMBER_CELLS],
    /// The fronting read-side value cache (Cut 35 slice 11): (id, name,
    /// generation, value) — a hit returns the value with no property-vector
    /// borrow or re-validation. Boxed so the Agent's hot-field cache
    /// footprint stays small (the Cut 27 lesson).
    pub(crate) member_value_cells:
        Box<[Option<crate::ir::MemberValueCell>; crate::ir::MEMBER_CELLS]>,
    /// The fronting array-element value cache (Cut 35 slice 13): (id, index,
    /// generation, value) — a hit returns the element with no
    /// property-vector borrow. Boxed per the Cut 27 lesson.
    pub(crate) array_element_value_cells:
        Box<[Option<crate::ir::ArrayElementValueCell>; crate::ir::MEMBER_CELLS]>,
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
    pub(crate) member_map_cells: [Option<crate::ir::MemberMapCell>; crate::ir::MEMBER_CELLS],
    pub(crate) array_element_cells:
        [Option<(u64, u64, crux::AtomId, usize)>; crate::ir::MEMBER_CELLS],
    /// The write-side chain cache (Cut 22): "the chain from this prototype
    /// holds no accessor/non-writable for this key" — the key's own
    /// property is absent on the receiver, so the store defines on it
    /// directly. Re-validated against the chain links' generations.
    pub(crate) member_store_cells: [Option<crate::ir::MemberStoreCell>; crate::ir::MEMBER_CELLS],
    /// The for-of fast-verdict cache (Cut 24): "the Array-iteration
    /// infrastructure is stock" — %Array.prototype%'s own @@iterator is the
    /// intrinsic, %ArrayIteratorPrototype% has the stock `next`, and no
    /// `return` on the AIP chain. Re-validated against the three shared
    /// objects' generations.
    pub(crate) for_of_fast_cells: [Option<ForOfFastVerdict>; crate::ir::MEMBER_CELLS],
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
    /// The leaf-inline cache (Cut 34): function id → the record data
    /// `do_call_fast` needs (compiled ir, strictness, closure env), so a hot
    /// leaf call skips the `ecma_functions` HashMap lookup. Boxed per the
    /// Cut 27 lesson.
    pub(crate) leaf_cache: Box<[Option<(u64, crate::ir::LeafEntry)>; crate::ir::LEAF_CACHE]>,
    /// The cached `prototype` read of each constructor function (Cut 26):
    /// `new C()` runs OrdinaryCreateFromConstructor's property read per
    /// construct; the value is re-validated against the function object's
    /// generation counter (Cut 22's mechanism — a redefine/delete bumps
    /// it), so a hot construct loop pays a HashMap probe instead of the
    /// full property path.
    pub(crate) construct_prototypes:
        Box<[Option<crate::ir::ConstructPrototypeCell>; crate::ir::CONSTRUCT_PROTO_CELLS]>,
    /// The free-list of Vms for per-call reuse: `run_compiled_body`, the
    /// construct fast path, and the script/eval paths take one, run, and
    /// return it — a pooled Vm is never handed to a suspended
    /// generator/async state (those own their Vm).
    pub(crate) vm_pool: Vec<crate::ir::Vm>,
    pub(crate) promise_jobs: VecDeque<Job>,
    pub(crate) generic_jobs: VecDeque<Job>,
    pub(crate) timeout_jobs: VecDeque<(Instant, Job)>,
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
    /// [[KeptAlive]]: objects/symbols kept alive until the end of the
    /// current Job (WeakRef, Phase 13).
    pub kept_alive: Vec<Value>,
    /// GC-2: a build-in-progress flag — native code accumulating
    /// handle-bearing values in local buffers (the class element build) sets
    /// it for the window; a `--gc-stress` collection that fires inside then
    /// aborts (retain everything), so the half-built buffers cannot be swept
    /// out from under the final record assignment.
    pub build_roots: std::cell::Cell<bool>,
    /// [[GlobalSymbolRegistry]]: `Symbol.for` entries (Phase 8).
    pub global_symbol_registry: RefCell<Vec<(JsString, Symbol)>>,
    /// [[ModuleAsyncEvaluationCount]]: module linking (Phase 7).
    pub module_async_evaluation_count: u32,
    /// The in-flight module evaluations (spec 16.2.2.5 InnerModuleEvaluation
    /// stack): used to detect dependency cycles and find the cycle root.
    pub module_eval_stack: Vec<crux::handle::Handle<crate::module::SourceTextModule>>,
    /// The ECMAScript-function bodies keyed by function identity (the spec
    /// 10.2.1 slots [[Environment]], [[FormalParameters]], [[ECMAScriptCode]],
    /// [[ThisMode]], [[HomeObject]] live here, Phase 7).
    pub ecma_functions: std::collections::HashMap<
        u64,
        crate::function::EcmaFunction,
        std::hash::BuildHasherDefault<IdentityHasher>,
    >,
    /// The Promise Records keyed by promise-object identity (spec 27.2.1).
    pub promises: std::collections::HashMap<u64, RefCell<crate::promise::PromiseData>>,
    /// The resolving functions created by CreateResolvingFunctions, keyed by
    /// function identity (spec 27.2.1.3).
    pub promise_resolvers:
        std::collections::HashMap<u64, std::rc::Rc<RefCell<crate::promise::ResolverData>>>,
    /// Per-element handlers created by the Promise combinators (all/
    /// allSettled/any), keyed by function identity.
    pub promise_compound: std::collections::HashMap<
        u64,
        std::rc::Rc<RefCell<crate::builtins::promise::CompoundState>>,
    >,
    /// The closures created by `Promise.prototype.finally`, keyed by function
    /// identity.
    pub promise_finally: std::collections::HashMap<
        u64,
        std::rc::Rc<RefCell<crate::builtins::promise::FinallyState>>,
    >,
    /// The await-resume handlers of running async functions, keyed by
    /// function identity.
    pub async_resume:
        std::collections::HashMap<u64, std::rc::Rc<crate::async_await::ResumeHandler>>,
    /// The AsyncFromSyncIterator methods, keyed by function identity.
    pub async_from_sync:
        std::collections::HashMap<u64, std::rc::Rc<crate::async_await::AsyncFromSyncEntry>>,
    /// The AsyncFromSyncIterator value-unwrap continuations, keyed by
    /// function identity (spec 27.1.5.4).
    pub async_from_sync_continuations: std::collections::HashMap<
        u64,
        std::rc::Rc<crate::async_await::AsyncFromSyncContinuationEntry>,
    >,
    /// The generator objects' states, keyed by object identity (spec 27.4.3).
    pub generators:
        std::collections::HashMap<u64, std::rc::Rc<RefCell<crate::generator::GeneratorState>>>,
    /// The async generator objects' states, keyed by object identity (spec
    /// 27.6.1).
    pub async_generators: std::collections::HashMap<
        u64,
        std::rc::Rc<RefCell<crate::async_generator::AsyncGeneratorState>>,
    >,
    /// The await-resume handlers of async generator bodies, keyed by function
    /// identity.
    pub async_generator_awaits: std::collections::HashMap<
        u64,
        std::rc::Rc<crate::async_generator::AsyncGeneratorAwaitEntry>,
    >,
    /// The iterator-helper objects' states, keyed by object identity (spec
    /// 27.1.3).
    pub iterator_helpers: std::collections::HashMap<
        u64,
        std::rc::Rc<RefCell<crate::builtins::iterator::HelperState>>,
    >,
    /// The wrapped iterators created by `Iterator.from`, keyed by object
    /// identity (spec 27.1.3.2).
    pub wrapped_iterators: std::collections::HashMap<
        u64,
        std::rc::Rc<RefCell<crate::builtins::iterator::WrappedIteratorState>>,
    >,
    /// The async-iterator-helper objects' states, keyed by object identity
    /// (spec 27.1.4).
    pub async_iterator_helpers: std::collections::HashMap<
        u64,
        std::rc::Rc<RefCell<crate::builtins::async_iterator::HelperState>>,
    >,
    /// The await continuations of async-iterator helper drivers, keyed by
    /// function identity.
    pub async_iterator_awaits:
        std::collections::HashMap<u64, std::rc::Rc<crate::builtins::async_iterator::AwaitEntry>>,
    /// The drivers of eager async-iterator helpers, keyed by driver-object
    /// identity.
    pub async_iterator_eager: std::collections::HashMap<
        u64,
        std::rc::Rc<RefCell<crate::builtins::async_iterator::EagerState>>,
    >,
    /// The capability of the in-flight `next()` of each lazy async-iterator
    /// helper, keyed by helper-object identity.
    pub async_iterator_pending: std::collections::HashMap<u64, crate::promise::PromiseCapability>,
    /// The [[DisposableStackData]] of DisposableStack/AsyncDisposableStack
    /// instances, keyed by object identity (spec 27.4.2).
    pub disposable_stacks:
        std::collections::HashMap<u64, RefCell<crate::builtins::disposable::DisposableStackData>>,
    /// The in-flight `disposeAsync` drivers, keyed by driver-object identity.
    pub disposable_async_drivers: std::collections::HashMap<
        u64,
        std::rc::Rc<RefCell<crate::builtins::disposable::AsyncDisposalDriver>>,
    >,
    /// The capability of each in-flight `disposeAsync`, keyed by driver
    /// identity.
    pub disposable_async_caps: std::collections::HashMap<u64, crate::promise::PromiseCapability>,
    /// The `disposeAsync` continuations, keyed by function identity: the
    /// driver id and whether the awaited disposal promise rejected.
    pub disposable_async_cont: std::collections::HashMap<u64, (u64, bool)>,
    /// The in-flight disposal drivers of async bodies (`using` resources
    /// drained at body completion), keyed by driver-object identity.
    pub async_body_disposal: std::collections::HashMap<
        u64,
        std::rc::Rc<RefCell<crate::builtins::disposable::AsyncBodyDisposalDriver>>,
    >,
    /// The async-body disposal continuations, keyed by function identity:
    /// the driver id and whether the awaited disposal promise rejected.
    pub async_body_disposal_cont: std::collections::HashMap<u64, (u64, bool)>,
    /// Agent-dependent built-ins (`%eval%`-pattern functions that cannot run
    /// inside crux closures) dispatch by intrinsic identity in
    /// `function::call_inner`. The result of that linear chain is memoized
    /// per function id: `0` means no agent dispatch (a plain closure
    /// builtin), otherwise the index into the dispatch table.
    pub builtin_dispatch_cache: std::collections::HashMap<u64, u8>,
    /// Host-provided module sources, keyed by specifier (HostResolveImportedModule).
    pub host_modules:
        RefCell<std::collections::HashMap<crux::string::JsString, crate::module::HostModuleSource>>,
    /// The module behind each namespace object, keyed by object identity.
    pub module_namespaces: std::collections::HashMap<u64, Handle<crate::module::SourceTextModule>>,
    /// The module behind each deferred namespace object ([[Deferred]] =
    /// true), keyed by object identity.
    pub deferred_namespaces:
        std::collections::HashMap<u64, Handle<crate::module::SourceTextModule>>,
    /// The module behind each `%AbstractModuleSource%` instance, keyed by
    /// object identity.
    pub module_sources: std::collections::HashMap<u64, Handle<crate::module::SourceTextModule>>,
    /// The dynamic-import namespace resolvers, keyed by function identity.
    pub import_namespace_resolvers: std::collections::HashMap<u64, (Value, Value)>,
    /// DeferredModule `.then` continuations (import-defer): each waiter
    /// function id maps to (wait id, is-rejection); the wait state holds the
    /// remaining async-dependency countdown and the capability.
    pub deferred_module_waiter_fns: std::collections::HashMap<u64, (u64, bool)>,
    /// The wait state of an `import.defer(...).then(...)` promise: (remaining,
    /// capability resolve, capability reject, module).
    pub deferred_module_waits: std::collections::HashMap<u64, crate::module::DeferredWait>,
    /// The module behind each DeferredModule `.then` method (import-defer),
    /// keyed by function identity.
    pub deferred_module_thens:
        std::collections::HashMap<u64, Handle<crate::module::SourceTextModule>>,
    /// [[BooleanData]] of Boolean wrapper objects, keyed by object identity
    /// (spec 20.3.1: `new Boolean(v)` boxes the ToBoolean result).
    pub boolean_data: std::collections::HashMap<u64, bool>,
    /// [[SymbolData]] of Symbol wrapper objects, keyed by object identity
    /// (spec 20.4.1: `Object(sym)` boxes the symbol).
    pub symbol_data: std::collections::HashMap<u64, crux::symbol::Symbol>,
    /// [[NumberData]] of Number wrapper objects, keyed by object identity
    /// (spec 21.1.1: `new Number(v)` boxes the ToNumber result).
    pub number_data: std::collections::HashMap<u64, f64>,
    /// [[BigIntData]] of BigInt wrapper objects, keyed by object identity
    /// (spec 21.2.1: `Object(5n)` boxes the BigInt).
    pub bigint_data: std::collections::HashMap<u64, crux::BigInt>,
    /// [[DateValue]] of Date instances, keyed by object identity (spec
    /// 21.4.3: ms since the epoch).
    pub date_data: std::collections::HashMap<u64, f64>,
    /// The [[InitializedLocale]] records of Intl.Locale instances, keyed by
    /// object identity (ECMA-402 §15: the canonical locale string).
    pub intl_locale_data: std::collections::HashMap<u64, crate::builtins::intl::IntlLocaleRecord>,
    /// The [[InitializedNumberFormat]] records of Intl.NumberFormat
    /// instances, keyed by object identity (ECMA-402 §16: the resolved
    /// options).
    pub intl_number_format_data:
        std::collections::HashMap<u64, crate::builtins::intl::number_format::NumberFormatRecord>,
    /// The per-instance bound `format` functions: function id → the
    /// NumberFormat instance's object id (ECMA-402 §16.5.2 [[NumberFormat]]).
    pub intl_format_functions: std::collections::HashMap<u64, u64>,
    /// The [[InitializedPluralRules]] records of Intl.PluralRules
    /// instances, keyed by object identity (ECMA-402 §17: the plural type
    /// and the digit/notation options).
    pub intl_plural_rules_data:
        std::collections::HashMap<u64, crate::builtins::intl::plural_rules::PluralRulesRecord>,
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
    /// The [[InitializedDateTimeFormat]] records of Intl.DateTimeFormat
    /// instances, keyed by object identity (ECMA-402 §11).
    pub intl_date_time_format_data: std::collections::HashMap<
        u64,
        crate::builtins::intl::date_time_format::DateTimeFormatRecord,
    >,
    /// The per-instance bound `format` functions: function id → the
    /// DateTimeFormat instance's object id (ECMA-402 §11.3.3).
    pub intl_dtf_format_functions: std::collections::HashMap<u64, u64>,
    /// The [[InitializedCollator]] records of Intl.Collator instances,
    /// keyed by object identity (ECMA-402 §10).
    pub intl_collator_data:
        std::collections::HashMap<u64, crate::builtins::intl::collator::CollatorRecord>,
    /// The per-instance bound `compare` functions: function id → the
    /// Collator instance's object id (ECMA-402 §10.3.3).
    pub intl_collator_compare_functions: std::collections::HashMap<u64, u64>,
    /// The [[InitializedSegmenter]] records of Intl.Segmenter instances,
    /// keyed by object identity (ECMA-402 §19: locale, granularity).
    pub intl_segmenter_data:
        std::collections::HashMap<u64, crate::builtins::intl::segmenter::SegmenterRecord>,
    /// The [[SegmentsSegmenter]]/[[SegmentsString]] slots of Intl.Segmenter
    /// `segment()` results (ECMA-402 §19.5).
    pub intl_segments_data:
        std::collections::HashMap<u64, crate::builtins::intl::segmenter::SegmentsRecord>,
    /// The [[IteratingSegmenter]]/[[IteratedString]]/
    /// [[IteratedStringNextSegmentCodeUnitIndex]] slots of segment
    /// iterators (ECMA-402 §19.6).
    pub intl_segment_iterator_data:
        std::collections::HashMap<u64, crate::builtins::intl::segmenter::SegmentIteratorRecord>,
    /// The [[InitializedDurationFormat]] records of Intl.DurationFormat
    /// instances, keyed by object identity (ECMA-402 §13: locale,
    /// numberingSystem, style, the per-unit options, fractionalDigits).
    pub intl_duration_format_data: std::collections::HashMap<
        u64,
        crate::builtins::intl::duration_format::DurationFormatRecord,
    >,
    /// The [[InitializedTemporal*]] records of Temporal instances, keyed by
    /// object identity (the proposal-temporal internal slots).
    pub temporal_data: std::collections::HashMap<u64, crate::builtins::temporal::TemporalRecord>,
    /// The [[Calendar]] internal slot of Temporal instances, keyed by object
    /// identity (the proposal-temporal calendar field; default "iso8601").
    pub temporal_calendars: std::collections::HashMap<u64, crux::string::JsString>,
    /// The RegExp internal state ([[OriginalSource]], [[OriginalFlags]],
    /// [[RegExpRecord]], [[RegExpMatcher]]) of RegExp instances, keyed by
    /// object identity (spec 22.2.5).
    pub regexp_data: std::collections::HashMap<u64, crate::builtins::regexp::RegExpState>,
    /// [[IteratingRegExp]], [[IteratedString]], [[Global]], [[Unicode]], and
    /// [[Done]] of RegExp String iterators (spec 22.2.6).
    pub regexp_string_iter_data:
        std::collections::HashMap<u64, (Value, crux::string::JsString, bool, bool, bool)>,
    /// [[IteratedString]] and [[StringIteratorNextIndex]] of String iterators,
    /// keyed by object identity (spec 22.1.5: the iterator's internal slots).
    pub string_iter_data: std::collections::HashMap<u64, (Option<crux::string::JsString>, u64)>,
    /// [[ErrorData]] markers of Error instances, keyed by object identity
    /// (spec 20.5.4: `Error.isError` and the `[object Error]` tag need it).
    pub error_data: std::collections::HashSet<u64>,
    /// The captured V8-style stack trace of Error instances, keyed by object
    /// identity (read by the `%Error.prototype.stack%` accessor; the property
    /// itself is not an own data property, spec 20.5.3.4).
    pub error_stack: std::collections::HashMap<u64, crux::string::JsString>,
    /// [[WeakRefTarget]] of WeakRef instances, keyed by object identity
    /// (spec 26.1.1: the target is held weakly — `deref` returns it while it
    /// is reachable, `undefined` once a collection clears it; GC-4).
    pub weak_ref_targets: std::cell::RefCell<std::collections::HashMap<u64, Value>>,
    /// The KeepDuringJob set (spec 26.1.1): WeakRef targets returned by
    /// `deref` in the current job stay alive until the job ends (the
    /// conservative stack scan covers the common case; this covers a deref
    /// result that is discarded immediately). Cleared at each job boundary.
    pub kept_during_job: std::cell::RefCell<Vec<Value>>,
    /// GC-4: FinalizationRegistry cleanup jobs enqueued by the collector's
    /// compaction hook (it cannot touch the job queues — the collector runs
    /// with `&self`). Moved into `generic_jobs` by the next job drain; the
    /// job closures' regions are scanned alongside the queued jobs.
    pub pending_cleanup_jobs: std::cell::RefCell<Vec<crate::job::Job>>,
    /// [[Cells]] and [[CleanupCallback]] of FinalizationRegistry instances,
    /// keyed by object identity (spec 26.2.1).
    pub finalization_registries: std::collections::HashMap<
        u64,
        std::rc::Rc<std::cell::RefCell<crate::builtins::weakref::FinalizationData>>,
    >,
    /// [[IteratedObject]], [[ArrayIteratorNextIndex]], and [[ArrayIterationKind]]
    /// of Array iterators, keyed by object identity (spec 23.1.5).
    pub array_iter_data: std::collections::HashMap<u64, (Value, usize, u32)>,
    /// The `Array.fromAsync` continuation states, keyed by handler-function
    /// identity (spec 23.1.2.4.1); the bool selects the reject handler.
    pub array_from_async: std::collections::HashMap<
        u64,
        (
            std::rc::Rc<std::cell::RefCell<crate::builtins::array::FromAsyncState>>,
            bool,
        ),
    >,
    /// The resolve functions of pending `Atomics.waitAsync` waits, keyed by
    /// the registered waiter-event id, with the (block id, byte offset) the
    /// event sits at in the global wait registry. A same-agent `Atomics.notify`
    /// resolves the promise directly with *"ok"* (spec 26.4.15 DoWait step
    /// 20); a cross-thread notify only marks the event, and the owning agent
    /// resolves it from its own thread (`service_wait_async` or the timeout
    /// job, whichever fires first).
    pub wait_async: std::collections::HashMap<u64, (Value, usize, usize)>,
    /// The internal state of ArrayBuffer/SharedArrayBuffer objects, keyed by
    /// object identity (spec 25.1.1: [[ArrayBufferData]], [[ArrayBufferByteLength]],
    /// [[ArrayBufferMaxByteLength]], and the resizable/growable + shared flags).
    /// Phase 12 created buffers as the TypedArray backing store; Phase 14 adds
    /// the full builtins.
    pub buffer_data:
        std::collections::HashMap<u64, RefCell<crate::builtins::array_buffer::BufferState>>,
    /// The [[ViewedArrayBuffer]], [[ByteLength]], and [[ByteOffset]] of
    /// DataView instances, keyed by object identity (spec 25.4.2).
    pub dataview_data:
        std::collections::HashMap<u64, RefCell<crate::builtins::dataview::DataViewState>>,
    /// The [[RawJSON]] text of JSON.rawJSON objects, keyed by object identity
    /// (spec 26.6.3).
    pub raw_json_data: std::collections::HashMap<u64, JsString>,
    /// The [[MapData]] of Map instances, keyed by object identity (spec
    /// 24.1.1: a List of entries; `None` marks a deleted ~empty~ slot that
    /// suspended Map iterators skip).
    pub map_data: std::collections::HashMap<u64, RefCell<Vec<MapEntry>>>,
    /// The [[SetData]] of Set instances, keyed by object identity (spec
    /// 24.2.1; `None` is a deleted ~empty~ slot).
    pub set_data: std::collections::HashMap<u64, RefCell<Vec<SetEntry>>>,
    /// The [[WeakMapData]] of WeakMap instances, keyed by object identity
    /// (spec 26.3.1; the Rc model never collects the keys, Phase 18).
    pub weak_map_data: std::collections::HashMap<u64, RefCell<Vec<MapEntry>>>,
    /// The [[WeakSetData]] of WeakSet instances, keyed by object identity
    /// (spec 26.4.1).
    pub weak_set_data: std::collections::HashMap<u64, RefCell<Vec<SetEntry>>>,
    /// The [[IteratedMap]], [[MapNextIndex]], and [[MapIterationKind]] of Map
    /// iterators, keyed by iterator-object identity (spec 24.1.6). The map
    /// value is `None` once iteration is done.
    pub map_iter_data: std::collections::HashMap<u64, RefCell<(Option<Value>, usize, u8)>>,
    /// The [[IteratedSet]], [[SetNextIndex]], and [[SetIterationKind]] of Set
    /// iterators, keyed by iterator-object identity (spec 24.2.6).
    pub set_iter_data: std::collections::HashMap<u64, RefCell<(Option<Value>, usize, u8)>>,
    /// The nesting depth of class field initializers currently evaluating.
    /// A direct eval inside one applies the "Eval Inside Initializer" early
    /// errors (spec 19.2.1.1): `arguments` is a SyntaxError there.
    pub field_initializer_depth: usize,
    /// Every realm created in this agent (the bootstrap realm plus any the
    /// host creates via `$262.createRealm`), so a realm's builtin called
    /// from another realm can dispatch with its own realm current.
    pub realms: RefCell<Vec<Handle<Realm>>>,
    /// The realm count as a plain `Cell` (Cut 35 slice 25): the leaf-inline
    /// eligibility checks read `realms.len()` on every call, and the
    /// RefCell borrow was measurable there; `realms` is only ever pushed
    /// (via `initialize_host_defined_realm`) and never popped or cleared,
    /// so a count cell stays exact, and `Cell` permits the write through
    /// the `&Agent` like `RefCell` does.
    pub realm_count: std::cell::Cell<usize>,
    /// Memoized owning-realm lookup: function id → the realm whose
    /// intrinsic table holds it (`None` for non-intrinsic functions).
    pub function_realms: RefCell<std::collections::HashMap<u64, Option<Handle<Realm>>>>,
    /// GC-1 slice 3: collect at every safe point when set (the `--gc-stress`
    /// mode; docs/gc-plan.md GC-2 hardens the root audit under it).
    pub gc_stress: Cell<bool>,
    /// The live-box count after the last collection, for the growth
    /// threshold that decides when a safe point collects.
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
}

impl Agent {
    pub fn new() -> Self {
        crate::function::ensure_ecma_hook();
        Self {
            execution_context_stack: Vec::new(),
            global_cells: [None; crate::ir::GLOBAL_CELLS],
            global_leaf_cells: Box::new(std::array::from_fn(|_| None)),
            slot_leaf_cells: Box::new(std::array::from_fn(|_| None)),
            member_cells: [None; crate::ir::MEMBER_CELLS],
            member_value_cells: Box::new(std::array::from_fn(|_| None)),
            array_element_value_cells: Box::new(std::array::from_fn(|_| None)),
            array_length_cells: Box::new(std::array::from_fn(|_| None)),
            member_proto_cells: [None; crate::ir::MEMBER_CELLS],
            member_map_cells: [const { None }; crate::ir::MEMBER_CELLS],
            array_element_cells: [None; crate::ir::MEMBER_CELLS],
            member_store_cells: [None; crate::ir::MEMBER_CELLS],
            for_of_fast_cells: std::array::from_fn(|_| None),
            for_of_array_cells: Box::new([None; crate::ir::MEMBER_CELLS]),
            leaf_cache: Box::new(std::array::from_fn(|_| None)),
            construct_prototypes: Box::new(std::array::from_fn(|_| None)),
            construct_property_patterns: Box::new(std::array::from_fn(|_| None)),
            vm_pool: Vec::new(),
            promise_jobs: VecDeque::new(),
            generic_jobs: VecDeque::new(),
            timeout_jobs: VecDeque::new(),
            host_hooks: None,
            little_endian: cfg!(target_endian = "little"),
            can_block: false,
            signifier: NEXT_AGENT_ID.fetch_add(1, Ordering::Relaxed),
            is_lock_free: [
                is_lock_free_for_size(1),
                is_lock_free_for_size(2),
                is_lock_free_for_size(8),
            ],
            kept_alive: Vec::new(),
            build_roots: std::cell::Cell::new(false),
            global_symbol_registry: RefCell::new(Vec::new()),
            module_async_evaluation_count: 0,
            module_eval_stack: Vec::new(),
            ecma_functions: std::collections::HashMap::default(),
            promises: std::collections::HashMap::new(),
            promise_resolvers: std::collections::HashMap::new(),
            promise_compound: std::collections::HashMap::new(),
            promise_finally: std::collections::HashMap::new(),
            async_resume: std::collections::HashMap::new(),
            async_from_sync: std::collections::HashMap::new(),
            async_from_sync_continuations: std::collections::HashMap::new(),
            generators: std::collections::HashMap::new(),
            async_generators: std::collections::HashMap::new(),
            async_generator_awaits: std::collections::HashMap::new(),
            iterator_helpers: std::collections::HashMap::new(),
            wrapped_iterators: std::collections::HashMap::new(),
            async_iterator_helpers: std::collections::HashMap::new(),
            async_iterator_awaits: std::collections::HashMap::new(),
            async_iterator_eager: std::collections::HashMap::new(),
            async_iterator_pending: std::collections::HashMap::new(),
            disposable_stacks: std::collections::HashMap::new(),
            disposable_async_drivers: std::collections::HashMap::new(),
            disposable_async_caps: std::collections::HashMap::new(),
            disposable_async_cont: std::collections::HashMap::new(),
            async_body_disposal: std::collections::HashMap::new(),
            async_body_disposal_cont: std::collections::HashMap::new(),
            builtin_dispatch_cache: std::collections::HashMap::new(),
            host_modules: RefCell::new(std::collections::HashMap::new()),
            module_namespaces: std::collections::HashMap::new(),
            deferred_namespaces: std::collections::HashMap::new(),
            module_sources: std::collections::HashMap::new(),
            import_namespace_resolvers: std::collections::HashMap::new(),
            deferred_module_waiter_fns: std::collections::HashMap::new(),
            deferred_module_waits: std::collections::HashMap::new(),
            deferred_module_thens: std::collections::HashMap::new(),
            boolean_data: std::collections::HashMap::new(),
            symbol_data: std::collections::HashMap::new(),
            number_data: std::collections::HashMap::new(),
            bigint_data: std::collections::HashMap::new(),
            date_data: std::collections::HashMap::new(),
            intl_locale_data: std::collections::HashMap::new(),
            intl_number_format_data: std::collections::HashMap::new(),
            intl_format_functions: std::collections::HashMap::new(),
            intl_plural_rules_data: std::collections::HashMap::new(),
            intl_rtf_data: std::collections::HashMap::new(),
            intl_list_format_data: std::collections::HashMap::new(),
            intl_display_names_data: std::collections::HashMap::new(),
            intl_date_time_format_data: std::collections::HashMap::new(),
            intl_dtf_format_functions: std::collections::HashMap::new(),
            intl_collator_data: std::collections::HashMap::new(),
            intl_collator_compare_functions: std::collections::HashMap::new(),
            intl_segmenter_data: std::collections::HashMap::new(),
            intl_segments_data: std::collections::HashMap::new(),
            intl_segment_iterator_data: std::collections::HashMap::new(),
            intl_duration_format_data: std::collections::HashMap::new(),
            temporal_data: std::collections::HashMap::new(),
            temporal_calendars: std::collections::HashMap::new(),
            regexp_data: std::collections::HashMap::new(),
            regexp_string_iter_data: std::collections::HashMap::new(),
            string_iter_data: std::collections::HashMap::new(),
            error_data: std::collections::HashSet::new(),
            error_stack: std::collections::HashMap::new(),
            weak_ref_targets: std::cell::RefCell::new(std::collections::HashMap::new()),
            kept_during_job: std::cell::RefCell::new(Vec::new()),
            pending_cleanup_jobs: std::cell::RefCell::new(Vec::new()),
            finalization_registries: std::collections::HashMap::new(),
            array_iter_data: std::collections::HashMap::new(),
            array_from_async: std::collections::HashMap::new(),
            wait_async: std::collections::HashMap::new(),
            buffer_data: std::collections::HashMap::new(),
            dataview_data: std::collections::HashMap::new(),
            raw_json_data: std::collections::HashMap::new(),
            map_data: std::collections::HashMap::new(),
            set_data: std::collections::HashMap::new(),
            weak_map_data: std::collections::HashMap::new(),
            weak_set_data: std::collections::HashMap::new(),
            map_iter_data: std::collections::HashMap::new(),
            set_iter_data: std::collections::HashMap::new(),
            field_initializer_depth: 0,
            realms: RefCell::new(Vec::new()),
            realm_count: std::cell::Cell::new(0),
            function_realms: RefCell::new(std::collections::HashMap::new()),
            gc_stress: Cell::new(false),
            last_collected_live: Cell::new(0),
        }
    }

    /// The running execution context: the top of the stack. Invariant: the
    /// stack is never empty after `initialize_host_defined_realm` has run.
    pub fn running_context(&self) -> Result<&ExecutionContext, JsError> {
        self.execution_context_stack.last().ok_or_else(|| {
            JsError::new(
                ErrorKind::ReferenceError,
                "No running execution context".into(),
            )
        })
    }

    pub fn running_context_mut(&mut self) -> Result<&mut ExecutionContext, JsError> {
        self.execution_context_stack.last_mut().ok_or_else(|| {
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
    ) -> crate::ir::Vm {
        match self.vm_pool.pop() {
            Some(mut vm) => {
                vm.reset(lexical_env, strict);
                vm
            }
            None => crate::ir::Vm::new(lexical_env, strict),
        }
    }

    /// Return a Vm to the pool after its run finished (the next run reuses
    /// its Vec capacities and inline frame).
    pub(crate) fn return_vm(&mut self, mut vm: crate::ir::Vm) {
        // GC-4: fully reset the pooled Vm's traceable state — a stale frame
        // slot, value-stack entry, or scope-env binding would otherwise keep
        // a WeakRef/FR target alive until the next run resets it (`vm_pool`
        // traces the pool). Re-pointing the env at the current context's
        // (itself a traced root) adds no retention.
        if let Ok(context) = self.running_context() {
            let env = context.lexical_environment;
            vm.reset(env, false);
        }
        self.vm_pool.push(vm);
    }

    /// The leaf-inline record for `id` (Cut 34): a direct-mapped cache over
    /// the `ecma_functions` map, so a hot leaf call skips the HashMap lookup.
    /// A miss resolves from the map and caches the entry (only for a
    /// leaf-inlineable function); function ids are never reused, so a hit
    /// needs no generation check.
    pub(crate) fn leaf_lookup(&mut self, id: u64) -> Option<&crate::ir::LeafEntry> {
        let index = id.wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize & (crate::ir::LEAF_CACHE - 1);
        let slot = &mut self.leaf_cache[index];
        if let Some((cached_id, _)) = slot
            && *cached_id == id
        {
            return slot.as_ref().map(|(_, entry)| entry);
        }
        let data = self.ecma_functions.get(&id)?;
        if !data.leaf_inline {
            return None;
        }
        let ir = data.ir.clone().expect("leaf_inline implies a compiled ir");
        let environment = if ir.leaf_uses_env {
            Some(data.environment)
        } else {
            None
        };
        *slot = Some((
            id,
            crate::ir::LeafEntry {
                ir,
                strict: data.strict,
                environment,
                // Cut 35 slice 15: the `Construct` step shares this cache,
                // so the construct-inline verdict rides along.
                construct_inline: data.construct_inline,
            },
        ));
        slot.as_ref().map(|(_, entry)| entry)
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
        let realm = initialize_host_defined_realm(self)?;
        self.push_bootstrap_context(realm);
        Ok(realm)
    }

    /// Push the initial execution context created in
    /// InitializeHostDefinedRealm: Function and ScriptOrModule are null.
    pub fn push_bootstrap_context(&mut self, realm: Handle<Realm>) {
        let global_env = realm.global_env;
        self.execution_context_stack.push(ExecutionContext {
            function: None,
            realm,
            script_or_module: None,
            lexical_environment: global_env,
            variable_environment: global_env,
            private_environment: None,
            source: None,
            annex_b_hoistable: Default::default(),
        });
    }

    /// The current Realm Record (the Realm component of the running context).
    pub fn current_realm(&self) -> Result<Handle<Realm>, JsError> {
        Ok(self.running_context()?.realm)
    }

    /// HostEnqueueGenericJob (spec 9.5.3): schedule a job without additional
    /// constraints such as priority.
    pub fn enqueue_generic_job(
        &mut self,
        realm: Option<Handle<Realm>>,
        closure: impl FnOnce(&mut Agent) -> Result<Value, JsError> + 'static,
    ) {
        self.generic_jobs.push_back(Job::new(realm, closure));
    }

    /// HostEnqueuePromiseJob (spec 9.5.4): schedule a job at promise
    /// priority. Jobs run in the order their enqueues happened.
    pub fn enqueue_promise_job(
        &mut self,
        realm: Option<Handle<Realm>>,
        closure: impl FnOnce(&mut Agent) -> Result<Value, JsError> + 'static,
    ) {
        self.promise_jobs.push_back(Job::new(realm, closure));
    }

    /// HostEnqueueTimeoutJob (spec 9.5.5): schedule a job to run after at
    /// least `milliseconds` milliseconds.
    pub fn enqueue_timeout_job(
        &mut self,
        realm: Option<Handle<Realm>>,
        milliseconds: u64,
        closure: impl FnOnce(&mut Agent) -> Result<Value, JsError> + 'static,
    ) {
        let deadline = Instant::now() + std::time::Duration::from_millis(milliseconds);
        self.timeout_jobs
            .push_back((deadline, Job::new(realm, closure)));
    }

    /// RunJobs: drain the job queues — promise jobs first (FIFO), then due
    /// timeouts, then generic jobs — until nothing runnable remains.
    pub fn run_jobs(&mut self) -> Result<(), JsError> {
        crux::function::with_agent(self as *mut Agent as *mut (), || self.run_jobs_inner())
    }

    fn run_jobs_inner(&mut self) -> Result<(), JsError> {
        // GC-5: the job drain is a fresh execution unit — the safe-point
        // allocation budget must not leak in from the previous script/job.
        crux::heap::reset_allocation_budget();
        // GC-4: promote the FinalizationRegistry cleanup jobs enqueued by the
        // collector's compaction hook into the generic queue (the hook runs
        // with `&self` and cannot touch the queues directly).
        self.generic_jobs
            .extend(self.pending_cleanup_jobs.borrow_mut().drain(..));
        loop {
            if let Some(job) = self.promise_jobs.pop_front() {
                self.run_job(job)?;
                continue;
            }
            let now = Instant::now();
            if let Some(index) = self
                .timeout_jobs
                .iter()
                .position(|(deadline, _)| *deadline <= now)
            {
                let Some((_, job)) = self.timeout_jobs.remove(index) else {
                    continue;
                };
                self.run_job(job)?;
                continue;
            }
            if let Some(job) = self.generic_jobs.pop_front() {
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
        let result = (job.closure)(self);
        RUNNING_JOB_REGION.with(|slot| *slot.borrow_mut() = None);
        // GC-4: the job ended — its KeepDuringJob set is no longer needed.
        self.kept_during_job.borrow_mut().clear();
        result
    }

    pub fn job_queues_empty(&self) -> bool {
        self.promise_jobs.is_empty() && self.generic_jobs.is_empty() && self.timeout_jobs.is_empty()
    }

    /// Parse and evaluate a Script (spec 16.1.4-16.1.6) in the current
    /// realm, returning the script's completion value.
    pub fn run_script(&mut self, source: &str) -> Result<Value, JsError> {
        crux::function::with_agent(self as *mut Agent as *mut (), || {
            // GC-5: a fresh script is a fresh execution unit — the safe-point
            // allocation budget must not leak in from the previous script.
            crux::heap::reset_allocation_budget();
            let realm = self.current_realm()?;
            let script = crate::script::parse_script(source, realm)?;
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

    /// GC-1 slice 3: every `Value`/`Handle`/`JsString`/`Symbol` the agent
    /// holds directly — the JS-visible roots of docs/gc-plan.md §3. The
    /// conservative native-stack scan covers Rust-held handles; this covers
    /// the agent's own tables (which live in heap-allocated buffers the
    /// stack scan cannot see). Index-only caches and primitive tables need
    /// no tracing.
    pub(crate) fn trace_roots(&self, visit: &mut dyn FnMut(GcAny)) {
        self.execution_context_stack.trace(visit);
        // The IC value caches hold Values; the index-only caches re-resolve
        // from the (traced) objects and need no tracing.
        for cell in self.member_value_cells.iter() {
            cell.trace(visit);
        }
        for cell in self.array_element_value_cells.iter() {
            cell.trace(visit);
        }
        for cell in self.for_of_fast_cells.iter() {
            cell.trace(visit);
        }
        for cell in self.global_leaf_cells.iter() {
            cell.trace(visit);
        }
        for cell in self.slot_leaf_cells.iter() {
            cell.trace(visit);
        }
        for (_, entry) in self.leaf_cache.iter().flatten() {
            entry.trace(visit);
        }
        // The cells below are RefCells: a per-allocation `--gc-stress`
        // collection can fire while one is mutably borrowed, so read them
        // Cut 26: the cached constructor-prototype reads hold Values.
        for cell in self.construct_prototypes.iter().flatten() {
            cell.value.trace(visit);
        }
        self.vm_pool.trace(visit);
        self.promise_jobs.trace(visit);
        self.generic_jobs.trace(visit);
        for (_, job) in &self.timeout_jobs {
            job.trace(visit);
        }
        self.kept_alive.trace(visit);
        self.global_symbol_registry.trace(visit);
        self.module_eval_stack.trace(visit);
        self.ecma_functions.trace(visit);
        self.promises.trace(visit);
        self.promise_resolvers.trace(visit);
        self.promise_compound.trace(visit);
        self.promise_finally.trace(visit);
        self.async_resume.trace(visit);
        self.async_from_sync.trace(visit);
        self.async_from_sync_continuations.trace(visit);
        self.generators.trace(visit);
        self.async_generators.trace(visit);
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
        self.symbol_data.trace(visit);
        self.bigint_data.trace(visit);
        self.intl_number_format_data.trace(visit);
        self.intl_plural_rules_data.trace(visit);
        self.intl_date_time_format_data.trace(visit);
        self.intl_collator_data.trace(visit);
        self.intl_segments_data.trace(visit);
        self.intl_segment_iterator_data.trace(visit);
        self.temporal_data.trace(visit);
        self.temporal_calendars.trace(visit);
        self.regexp_data.trace(visit);
        for (value, text, _, _, _) in self.regexp_string_iter_data.values() {
            value.trace(visit);
            text.trace(visit);
        }
        for (text, _) in self.string_iter_data.values() {
            text.trace(visit);
        }
        self.error_stack.trace(visit);
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
        for data in self.finalization_registries.values() {
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
        for (value, _, _) in self.array_iter_data.values() {
            value.trace(visit);
        }
        for (state, _) in self.array_from_async.values() {
            state.trace(visit);
        }
        for (value, _, _) in self.wait_async.values() {
            value.trace(visit);
        }
        self.dataview_data.trace(visit);
        self.raw_json_data.trace(visit);
        self.map_data.trace(visit);
        self.set_data.trace(visit);
        // GC-3: WeakMap/WeakSet entries are ephemerons — the key (and thus
        // the entry) lives only while it is reachable from other roots, and
        // the value lives only while its key does. Register the edges for
        // the collector's fixpoint instead of tracing them strongly; the
        // post-collection compaction drops entries whose key was swept.
        for cell in self.weak_map_data.values() {
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
        for cell in self.weak_set_data.values() {
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
        for iter_data in self.map_iter_data.values() {
            let Ok(guard) = iter_data.try_borrow() else {
                crux::heap::note_aborted_trace();
                return;
            };
            let (value, _, _) = &*guard;
            value.trace(visit);
        }
        for iter_data in self.set_iter_data.values() {
            let Ok(guard) = iter_data.try_borrow() else {
                crux::heap::note_aborted_trace();
                return;
            };
            let (value, _, _) = &*guard;
            value.trace(visit);
        }
        self.realms.trace(visit);
        self.function_realms.trace(visit);
        // GC-2: the Vms currently running bodies (their heap-buffered value
        // stacks are invisible to the conservative stack scan). Pooled and
        // suspended Vms are covered by `vm_pool` and the generator/async
        // tables above.
        crate::ir::trace_active_vms(visit);
        // GC-2: the opaque job closures hold captured Values; scan their
        // boxes conservatively (queued jobs plus the job currently running).
        let mut regions: Vec<(*const u8, usize)> = Vec::new();
        for job in &self.promise_jobs {
            regions.push(job.closure_region());
        }
        for job in &self.generic_jobs {
            regions.push(job.closure_region());
        }
        for (_, job) in &self.timeout_jobs {
            regions.push(job.closure_region());
        }
        // GC-4: the pending FinalizationRegistry cleanup jobs (captured
        // held values and callbacks) ride alongside the queued jobs.
        if let Ok(pending) = self.pending_cleanup_jobs.try_borrow() {
            for job in pending.iter() {
                regions.push(job.closure_region());
            }
        } else {
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
        self.trace_roots(&mut |any| roots.push(any));
        // GC-3/GC-4: the compaction hook runs between the mark and the sweep
        // (the would-be-swept boxes are still allocated), dropping dead weak
        // entries and capturing FinalizationRegistry held values into
        // cleanup jobs. The `precise` flag requests a scan-free mark for the
        // compaction's dead set: stale stack words must not keep a WeakRef
        // or FinalizationRegistry target alive (the sweep still uses the
        // conservative mark — the scan stays the safety net for Rust-held
        // handles).
        let has_weak = self.has_weak_structures();
        // Cut 35 slice 31: the compaction hook (dead-set HashSet build +
        // weak-table walks) runs only when a weak structure actually exists
        // — the benchmark's collections have none, and the SipHash HashSet
        // of every dead address was measurable per collection.
        let compact: &mut crux::heap::CompactHook = if has_weak {
            &mut |dead, retain| self.compact_weak_tables(dead, retain)
        } else {
            &mut |_, _| {}
        };
        let swept = crux::heap::with_heap_mut(|heap| {
            heap.collect_with_stack_compacting(&roots, has_weak, compact)
                .len()
        });
        self.last_collected_live
            .set(crux::heap::with_heap(|heap| heap.live_count()));
        crux::heap::note_collection(swept);
    }

    /// GC-4: whether any weak structure exists — the collector then runs a
    /// scan-free mark so the weak tables compact against true heap
    /// reachability instead of stale stack words. A `RefCell` mid-borrow
    /// (the tables are read-only here) defaults to `true`: the precise pass
    /// is harmless when the tables are empty.
    fn has_weak_structures(&self) -> bool {
        !self.weak_map_data.is_empty()
            || !self.weak_set_data.is_empty()
            || !self.finalization_registries.is_empty()
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
        for cell in self.weak_map_data.values() {
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
        for cell in self.weak_set_data.values() {
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
        let Ok(realm) = self.current_realm() else {
            return;
        };
        for data in self.finalization_registries.values() {
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

    /// The safe-point collection trigger: collect when the heap has grown
    /// past twice the post-collection live count (or every safe point in
    /// `--gc-stress` mode).
    pub(crate) fn maybe_collect(&mut self) {
        let live = crux::heap::with_heap(|heap| heap.live_count());
        let threshold = self.last_collected_live.get().max(1024).saturating_mul(2);
        if self.gc_stress.get() || live > threshold {
            self.collect_garbage();
        }
    }

    /// Toggle the `--gc-stress` mode: collect at every safe point instead of
    /// only on heap growth. Settable through `&Agent` (the cell).
    pub fn set_gc_stress(&self, enabled: bool) {
        self.gc_stress.set(enabled);
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
