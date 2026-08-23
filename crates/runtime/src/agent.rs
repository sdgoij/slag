//! Agents (spec 9.7) and the surrounding-agent operations.
//!
//! The agent owns the execution context stack and the job queues; its
//! record fields ([[]] names below) are the Agent Record fields of the
//! spec 9.7 table. Single-threaded: [[CanBlock]] is false, so
//! AgentCanSuspend() is false.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::string::JsString;
use crux::symbol::Symbol;
use crux::value::Value;

use crate::context::ExecutionContext;
use crate::host::HostHooks;
use crate::job::Job;
use crate::realm::{Realm, initialize_host_defined_realm};

static NEXT_AGENT_ID: AtomicU64 = AtomicU64::new(1);

/// One entry of a Map/WeakMap `[[*Data]]` List: `None` is the ~empty~
/// (deleted) slot, `Some((key, value))` a live entry.
pub type MapEntry = Option<(Value, Value)>;

/// One element of a Set/WeakSet `[[*Data]]` List.
pub type SetEntry = Option<Value>;

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
    /// The proto-keyed read-cell fallback (Cut 23): `(prototype id, name) →
    /// slot` — fresh objects (a constructor's new `this`) share the
    /// prototype's shape, so the slot cached for the prototype is validated
    /// against each object's own property vector on access (a divergent
    /// layout misses and re-resolves). Self-validating like `member_cells`.
    pub(crate) member_proto_cells: [Option<(u64, crux::AtomId, usize)>; crate::ir::MEMBER_CELLS],
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
    pub(crate) construct_prototypes: RefCell<std::collections::HashMap<u64, (u32, Value)>>,
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
    /// (spec 26.1.1: without a GC, the target never dies, so `deref` always
    /// returns it).
    pub weak_ref_targets: std::collections::HashMap<u64, Value>,
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
    /// Memoized owning-realm lookup: function id → the realm whose
    /// intrinsic table holds it (`None` for non-intrinsic functions).
    pub function_realms: RefCell<std::collections::HashMap<u64, Option<Handle<Realm>>>>,
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
            member_proto_cells: [None; crate::ir::MEMBER_CELLS],
            array_element_cells: [None; crate::ir::MEMBER_CELLS],
            member_store_cells: [None; crate::ir::MEMBER_CELLS],
            for_of_fast_cells: std::array::from_fn(|_| None),
            for_of_array_cells: Box::new([None; crate::ir::MEMBER_CELLS]),
            leaf_cache: Box::new(std::array::from_fn(|_| None)),
            construct_prototypes: RefCell::new(std::collections::HashMap::new()),
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
            weak_ref_targets: std::collections::HashMap::new(),
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
            function_realms: RefCell::new(std::collections::HashMap::new()),
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
    pub(crate) fn return_vm(&mut self, vm: crate::ir::Vm) {
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
            Some(data.environment.clone())
        } else {
            None
        };
        *slot = Some((
            id,
            crate::ir::LeafEntry {
                ir,
                strict: data.strict,
                environment,
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
        self.push_bootstrap_context(realm.clone());
        Ok(realm)
    }

    /// Push the initial execution context created in
    /// InitializeHostDefinedRealm: Function and ScriptOrModule are null.
    pub fn push_bootstrap_context(&mut self, realm: Handle<Realm>) {
        let global_env = realm.global_env.clone();
        self.execution_context_stack.push(ExecutionContext {
            function: None,
            realm,
            script_or_module: None,
            lexical_environment: global_env.clone(),
            variable_environment: global_env,
            private_environment: None,
            source: None,
            annex_b_hoistable: Default::default(),
        });
    }

    /// The current Realm Record (the Realm component of the running context).
    pub fn current_realm(&self) -> Result<Handle<Realm>, JsError> {
        Ok(self.running_context()?.realm.clone())
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
        Ok(())
    }

    fn run_job(&mut self, job: Job) -> Result<Value, JsError> {
        (job.closure)(self)
    }

    pub fn job_queues_empty(&self) -> bool {
        self.promise_jobs.is_empty() && self.generic_jobs.is_empty() && self.timeout_jobs.is_empty()
    }

    /// Parse and evaluate a Script (spec 16.1.4-16.1.6) in the current
    /// realm, returning the script's completion value.
    pub fn run_script(&mut self, source: &str) -> Result<Value, JsError> {
        crux::function::with_agent(self as *mut Agent as *mut (), || {
            let realm = self.current_realm()?;
            let script = crate::script::parse_script(source, realm)?;
            crate::script::script_evaluation(self, &script)
        })
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
