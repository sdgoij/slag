//! The wasm execution machine (spec ch. 4) over a [`Store`] of module
//! instances (spec ch. 4.2).
//!
//! Instances share the store's mutable state: globals, linear memories, and
//! tables are cells in store-owned pools, so a module that imports another
//! instance's memory, mutable global, or table aliases the same cell. Function
//! imports bind to a flattened target (a defined body in some instance, or a
//! host function).
//!
//! One shared operand stack plus an explicit stack of function frames (each
//! with its own instance, pc, locals, and control labels) keeps call depth
//! bounded by a configured limit instead of the host stack. Numeric semantics
//! live in [`crate::values`].
//!
//! Not executed yet ([`ExecFail::Unsupported`]): threads and the atomic
//! memory operations (a separate proposal outside the pinned corpus).

use crate::instr::{Catch, Instr, LoadOp, NumOp, StoreOp, VecLoadOp};
use crate::module::{DataMode, ElementMode, ExportKind, ImportDesc, Module};
use crate::types::{
    BlockType, CompositeType, FieldType, FuncType, GlobalType, HeapType, Limits, MemType, RefType,
    StorageType, TableType, ValType,
};
use crate::values::{ExternInner, FuncAddr, RefValue, Trap, Value, exec_num};

/// How execution stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecFail {
    Trap(Trap),
    /// A feature this cut does not execute yet (later cuts).
    Unsupported(&'static str),
    /// An uncaught wasm exception, thrown by `throw`/`throw_ref` and not
    /// handled by any `try_table`: the id of the exception in the store's
    /// pool. Only the outermost invocation edge sees this — the run loop
    /// converts it into a branch whenever a matching catch clause exists.
    Exception(usize),
}

impl From<Trap> for ExecFail {
    fn from(trap: Trap) -> Self {
        ExecFail::Trap(trap)
    }
}

/// How instantiation stopped, split by the .wast command that asserts it:
/// [`InstantiateError::Unlinkable`] (import resolution/type failure),
/// [`InstantiateError::Trap`] (a trap during instantiation), or an
/// unsupported feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstantiateError {
    Unlinkable(&'static str),
    Trap(Trap),
    Unsupported(&'static str),
}

impl From<Trap> for InstantiateError {
    fn from(trap: Trap) -> Self {
        InstantiateError::Trap(trap)
    }
}

impl From<ExecFail> for InstantiateError {
    fn from(fail: ExecFail) -> Self {
        match fail {
            ExecFail::Trap(trap) => InstantiateError::Trap(trap),
            ExecFail::Unsupported(reason) => InstantiateError::Unsupported(reason),
            ExecFail::Exception(_) => {
                InstantiateError::Unsupported("exception during instantiation")
            }
        }
    }
}

impl std::fmt::Display for InstantiateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstantiateError::Unlinkable(reason) => write!(f, "link error: {reason}"),
            InstantiateError::Trap(trap) => write!(f, "trap: {trap:?}"),
            InstantiateError::Unsupported(reason) => write!(f, "unsupported: {reason}"),
        }
    }
}

/// A host call the resumable driver must resolve: an *external* host function
/// (registered by [`Store::external_host`] — a JS-API function import) was
/// called from running wasm. The driver runs the embedder's function with
/// `args` while the store is not borrowed, then hands the results back to
/// [`Store::resume`].
#[derive(Debug)]
pub struct HostRequest {
    /// The token [`Store::external_host`] was registered with; the JS-API
    /// maps it back to the JS function it wraps.
    pub token: u64,
    /// The function's arguments, in parameter order.
    pub args: Vec<Value>,
}

/// What a resumable run produced at its next host-call boundary (or the
/// end).
#[derive(Debug)]
pub enum RunProgress {
    /// The invocation finished with these results.
    Finished(Vec<Value>),
    /// The invocation needs an external host call (see [`HostRequest`]).
    Host(HostRequest),
}

/// Default call-depth budget (the corpus's `assert_exhaustion` expects a
/// bounded stack).
pub const DEFAULT_DEPTH_LIMIT: usize = 4096;

/// Cut 11: compiled-to-compiled native re-entry depth budget. Each native
/// level costs a Cranelift frame plus Rust helper/trampoline frames; past
/// this the call helper runs callees through the interpreter (whose own frame
/// budget then applies), so runaway recursion traps like an interpreted run
/// instead of overflowing the native stack. Kept small enough that even the
/// debug build (and a 1 MB host main-thread stack) survives the worst corpus
/// recursion.
pub const NATIVE_CALL_DEPTH: usize = 64;

/// The page size of linear memory, in bytes.
pub const PAGE_SIZE: u64 = 65536;

/// A linear memory: growable byte storage plus its declared maximum.
#[derive(Debug)]
pub struct Memory {
    pub bytes: Vec<u8>,
    max_pages: Option<u64>,
}

impl Memory {
    /// A zero-filled memory of `min_pages` pages, capped at `max_pages`.
    pub fn new(min_pages: u64, max_pages: Option<u64>) -> Self {
        Memory {
            bytes: vec![0; (min_pages * PAGE_SIZE) as usize],
            max_pages,
        }
    }

    pub fn pages(&self) -> u64 {
        self.bytes.len() as u64 / PAGE_SIZE
    }

    /// Grow by `delta` pages; returns the old size in pages, or `None` when
    /// the growth would exceed the declared maximum (a memory32 is also
    /// capped at 2^16 pages — 4 GiB — even without a declared maximum;
    /// memory64 is only bounded by what a byte vector can address).
    pub fn grow(&mut self, delta: u64, memory64: bool) -> Option<u64> {
        let old = self.pages();
        let new = old.checked_add(delta)?;
        let cap = match self.max_pages {
            Some(max) => max,
            None if memory64 => (usize::MAX / PAGE_SIZE as usize) as u64,
            None => 1 << 16,
        };
        if new > cap {
            return None;
        }
        let byte_len = new.checked_mul(PAGE_SIZE)?;
        let Ok(byte_len) = usize::try_from(byte_len) else {
            return None;
        };
        self.bytes.resize(byte_len, 0);
        Some(old)
    }
}

/// A table instance: reference slots plus its declared maximum.
#[derive(Debug)]
pub struct TableInst {
    elements: Vec<RefValue>,
    max: Option<u64>,
}

impl TableInst {
    fn new(min: u64, max: Option<u64>, init: RefValue) -> Self {
        TableInst {
            elements: vec![init; min as usize],
            max,
        }
    }

    /// Grow by `delta` slots, filling with `init`; returns the old size, or
    /// `None` when the growth would exceed the limits (a 32-bit table is
    /// capped at 2^32-1 slots even without a declared maximum).
    fn grow(&mut self, delta: u64, init: RefValue, table64: bool) -> Option<u64> {
        let old = self.elements.len() as u64;
        let new = old.checked_add(delta)?;
        let cap = self.max.unwrap_or(if table64 {
            usize::MAX as u64
        } else {
            u64::from(u32::MAX)
        });
        if new > cap {
            return None;
        }
        self.elements.resize(new as usize, init);
        Some(old)
    }
}

/// A tag instance: the exception's payload shape (its function type's
/// parameters; the results are empty by validation).
#[derive(Debug)]
pub struct TagInst {
    ty: FuncType,
    /// The instance and module type index that declared the tag, when it came
    /// from a module (used for cross-module import matching); `None` for
    /// standalone JS-API tags.
    owner: Option<(usize, u32)>,
}

/// A concrete exception: which tag was thrown plus the argument values.
/// Both in-flight exceptions (unwinding to a catch) and `exnref` values
/// reference these cells by id.
#[derive(Debug)]
pub struct ExceptionInst {
    pub tag: usize,
    pub args: Vec<Value>,
}

/// A GC aggregate object (struct or array) in the store's object pool. The
/// owning instance's module owns `ty`, which fixes the field/element storage
/// layout; each data slot is a [`Value`] (packed cells keep the wrapped
/// unsigned bits). Objects are append-only, so ids stay stable.
#[derive(Debug)]
pub struct GcObject {
    /// The instance whose module allocated this object (its type space owns
    /// `ty`).
    pub owner: usize,
    /// The object's type index in the owner's module.
    pub ty: u32,
    pub data: GcData,
}

#[derive(Debug)]
pub enum GcData {
    /// One value per declared struct field.
    Struct(Vec<Value>),
    /// One value per array element.
    Array(Vec<Value>),
}

/// A value handed to [`Store::instantiate`] for one import. Function, global,
/// memory, table, and tag values reference cells/instances that stay alive in
/// the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternVal {
    /// A host function (e.g. the spectest `print*` family).
    HostFunc(usize),
    /// The function at `index` in the full function space of `instance`.
    Func { instance: usize, index: usize },
    /// A shared global cell.
    Global(usize),
    /// A shared memory cell.
    Memory(usize),
    /// A shared table cell.
    Table(usize),
    /// A shared tag cell.
    Tag(usize),
    /// The value exists but its kind is not executable this cut.
    Unsupported(&'static str),
}

/// The canonical identity of a function: instance function spaces alias the
/// target they resolve to, so one function surfaced through any number of
/// import/export chains shares a key (the JS-API's wrapper-object memo).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FuncKey {
    /// A host function (the spectest `print*` family or an external JS
    /// closure registered through [`Store::external_host`]).
    Host(usize),
    /// The `defined`-th function declared by `instance`.
    Owned { instance: usize, defined: usize },
}

/// Where a function index ultimately executes: a host routine or a defined
/// body of some instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FuncTarget {
    Host(usize),
    Owned { instance: usize, defined: usize },
}

/// A host function: the spectest `print*` family is type-only (no body, no
/// results), while the JS-API's function imports are *external* — their
/// results come from the embedder when a resumable run reaches a
/// [`HostRequest`].
struct HostFunc {
    ty: FuncType,
    token: Option<u64>,
}

/// A module instance: the module (code, types, exports) plus its index
/// spaces. Globals, memories, tables, and tags are cell ids into the owning
/// store, so imports alias the exporter's cells; functions are flattened
/// [`FuncTarget`]s. Data and element segments are retained per instance for
/// the bulk-memory instructions (active segments are dropped after use).
pub struct Instance {
    module: Module,
    funcs: Vec<FuncTarget>,
    tables: Vec<usize>,
    globals: Vec<usize>,
    memories: Vec<usize>,
    tags: Vec<usize>,
    element_segments: Vec<Option<Vec<RefValue>>>,
    data_segments: Vec<Option<Vec<u8>>>,
    depth_limit: usize,
    /// Cut 11: one optional compiled entry per module-defined function
    /// (parallel to `Module::bodies`); `None` keeps the interpreter path.
    #[cfg(feature = "compile")]
    compiled: Vec<Option<crate::compile::CompiledFunc>>,
    /// Cut 11 Wave 3: per module-defined body (parallel to `Module::bodies`),
    /// whether its direct callee graph can reach an external host function (a
    /// function import resolved to a token'd host). The compiled path cannot
    /// suspend at an external host boundary, so such bodies run interpreted;
    /// later instances importing a function of this one consult this when
    /// deciding their own bodies.
    #[cfg(feature = "compile")]
    host_reachable: Vec<bool>,
}

/// The store: every live instance plus the shared pools they reference.
pub struct Store {
    pub instances: Vec<Instance>,
    host_funcs: Vec<HostFunc>,
    globals: Vec<Value>,
    global_types: Vec<GlobalType>,
    memories: Vec<Memory>,
    memory_types: Vec<MemType>,
    tables: Vec<TableInst>,
    table_types: Vec<TableType>,
    tags: Vec<TagInst>,
    exceptions: Vec<ExceptionInst>,
    /// Live GC struct/array objects; ids double as `RefValue::Struct/Array`
    /// payloads.
    objects: Vec<GcObject>,
    /// Runs suspended at an external-host-call boundary, innermost last
    /// ([`Store::start`] parks here; [`Store::resume`] pops and continues).
    suspended: Vec<Suspended>,
    /// Cut 11 test hook: when true, even compiled bodies run on the
    /// interpreter (lets the equivalence tests compare both paths).
    #[cfg(feature = "compile")]
    compile_off: bool,
    /// Cut 11: an error the call helper could not express as a trap code
    /// (an external host boundary, an escaping exception, an unsupported
    /// form), parked here so the compiled caller surfaces the exact error
    /// the interpreter would have produced.
    #[cfg(feature = "compile")]
    pending_error: Option<ExecFail>,
    /// Cut 11: current compiled-to-compiled native call depth. Above
    /// [`NATIVE_CALL_DEPTH`] the call helper routes callees through the
    /// interpreter instead, bounding the native stack so the interpreter's
    /// exhaustion semantics still hold for runaway recursion.
    #[cfg(feature = "compile")]
    native_depth: usize,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

/// Cut 11: the `memory.grow` runtime helper called from compiled bodies
/// through a helper-signature `call_indirect` (its address rides the entry
/// ABI as param 9). `memory` is a module memory index (imports first); the
/// helper resolves it to the instance's store cell, grows the backing `Vec`
/// through the store (enforcing the cell's declared maximum and the memory32
/// 2^16-page cap), then rewrites the caller-owned descriptor entry at `desc`
/// — data pointer, then byte length — so later compiled accesses see the
/// reallocated buffer (a `Vec` data pointer is unstable across a resize).
/// Returns the old page count, or `u64::MAX` (-1) when the growth would
/// exceed the limits (the descriptor is then untouched).
///
/// # Safety
///
/// The caller (a compiled body running under [`Store::start_owned`]) holds
/// `&mut self` for the whole call, so `store` stays valid and uniquely
/// borrowed; `desc` points into the caller-owned descriptor array, which
/// stays alive for the call. The instance/memory indices come from a
/// validated module, so the store-cell lookups always resolve.
#[cfg(feature = "compile")]
pub unsafe extern "C" fn memory_grow_helper(
    store: *mut Store,
    instance: u64,
    memory: u64,
    delta: u64,
    desc: *mut u64,
) -> u64 {
    // SAFETY: the caller (a compiled body running under `Store::start_owned`)
    // holds `&mut self` for the whole call, so the raw store pointer stays
    // valid and uniquely borrowed; `desc` points into the caller-owned
    // descriptor array, which stays alive for the call. The instance/memory
    // indices come from a validated module, so the lookups always resolve.
    let store = unsafe { &mut *store };
    let cell = store
        .instances
        .get(instance as usize)
        .and_then(|inst| inst.memories.get(memory as usize))
        .copied();
    let Some(cell) = cell else {
        return u64::MAX;
    };
    match store.grow_memory(cell, delta) {
        Some(old) => {
            let grown = &store.memories[cell];
            unsafe {
                *desc = grown.bytes.as_ptr() as u64;
                *desc.add(1) = grown.bytes.len() as u64;
            }
            old
        }
        None => u64::MAX,
    }
}

/// Decode one u64 call slot into an interpreter value of `ty` (the compiled
/// entry ABI's argument marshaling, inverted). Numeric types decode directly;
/// a reference type decodes its token (the lowerable subset only carries
/// null/function/external/i31 references). Returns `None` for an unsupported
/// type or an unencodable token.
#[cfg(feature = "compile")]
fn value_from_call_slot(ty: ValType, slot: u64) -> Option<Value> {
    match ty {
        ValType::I32 => Some(Value::I32(slot as u32 as i32)),
        ValType::I64 => Some(Value::I64(slot as i64)),
        ValType::F32 => Some(Value::F32(slot as u32)),
        ValType::F64 => Some(Value::F64(slot)),
        // Any reference parameter decodes by token: the lowerable subset only
        // ever carries null/function/external/i31 references across a
        // boundary, and the token carries its own kind.
        ValType::Ref(_) => crate::values::token_to_ref(slot),
        _ => None,
    }
}

/// Encode an interpreter value into its u64 call slot (numeric types, plus
/// null/function references as their tokens).
#[cfg(feature = "compile")]
fn value_to_call_slot(value: Value) -> Option<u64> {
    match value {
        Value::I32(bits) => Some(bits as u32 as u64),
        Value::I64(bits) => Some(bits as u64),
        Value::F32(bits) => Some(u64::from(bits)),
        Value::F64(bits) => Some(bits),
        Value::Ref(_) => crate::values::ref_to_token(value),
        _ => None,
    }
}

/// Cut 11: the call helper for compiled bodies (its address rides the entry
/// ABI as param 10). It resolves the call target the same way the interpreter
/// would and runs it to completion through the interpreter engine — compiled
/// bodies call out rather than re-entering other compiled bodies, which keeps
/// the native stack bounded and preserves the interpreter's frame-depth and
/// host-boundary semantics exactly. Arguments arrive as u64 slots at
/// `scratch[0..params)`; the results overwrite `scratch[0..results)`. After
/// the callee runs (success or trap — wasm traps do not roll back growth)
/// the caller's memory descriptors at `mems` are refreshed from the store, so
/// later compiled accesses see any memory the callee grew. Returns a trap
/// code (0 = ok); a non-trap error is parked on the store and the
/// [`crate::compile::TRAP_PENDING_ERROR`] sentinel returned.
///
/// `mode` selects the operation: `0` = direct call to function index-space
/// entry `x` (`y`/`z` unused); `1` = `call_indirect` through table `x` at
/// element `y`, type-checked against `z`; `2` = `call_ref` on the function
/// token `x`, type-checked against `z`; `3` = `table.get` at table `x` slot
/// `y`, token written to `scratch[0]`; `4` = `table.set` of token `z` at
/// table `x` slot `y`; `5`-`8` = the bulk-memory ops (see [`bulk_op`]);
/// `9`-`14` = the table bulk ops (see [`table_bulk_op`]); `15` = a `throw`
/// of tag index-space entry `x` whose payload rides `scratch[0..nargs)`,
/// parked as an in-flight exception (see [`throw_op`]); `16` = a `throw_ref`
/// re-raising the exception whose pool id is `x` (see [`rethrow_op`]);
/// `17`/`18` = `global.get`/`global.set` over any global index-space entry
/// (see [`global_op`]);
/// `40`/`41` = `extern.convert_any`/`any.convert_extern` re-tagging the
/// token `x` (see [`gc_convert_op`]); `50`-`56` = the array bulk ops (see
/// [`array_bulk_op`]); `60` = the `ref.test`/`ref.cast` type match check
/// (see [`ref_cast_op`]); `70` = a v128 register op mirroring `simd_exec`
/// over operands in `scratch` (see [`simd_op`]); `71`-`74` = the v128
/// memory/shuffle ops (see [`v128_mem`]).
///
/// # Safety
///
/// The caller (a compiled body running under [`Store::start_owned`]) holds
/// `&mut self` for the whole call, so `store` stays valid and uniquely
/// borrowed; `scratch` and `mems` point into caller-owned buffers that stay
/// alive for the call. The instance/table/type indices come from a validated
/// module.
#[cfg(feature = "compile")]
pub unsafe extern "C" fn wasm_call_helper(
    store: *mut Store,
    instance: u64,
    mode: u64,
    x: u64,
    y: u64,
    z: u64,
    nargs: u64,
    scratch: *mut u64,
    mems: *mut u64,
) -> i32 {
    let code_of = crate::compile::code_of_trap;
    let store = unsafe { &mut *store };
    // A compiled `throw` (mode 15) or `throw_ref` (mode 16) parks an
    // in-flight exception as a pending error for the entry to drain (no
    // compiled body can contain a `try_table` catch, so the exception always
    // escapes the native path).
    if mode == 15 || mode == 16 {
        if mode == 15 {
            return throw_op(store, instance, x, nargs, scratch);
        }
        return rethrow_op(store, x);
    }
    // A compiled `global.get` (17) / `global.set` (18) accesses the store
    // cell directly (imported and call-bearing globals never ride the `gvals`
    // snapshot; see [`global_op`]).
    if mode == 17 || mode == 18 {
        return global_op(store, instance, mode, x, scratch);
    }
    // Compiled GC aggregate ops (modes 30-38) mirror the interpreter's
    // struct/array semantics against the store's object pool.
    if (30..=38).contains(&mode) {
        return gc_op(store, instance, mode, x, y, z, nargs, scratch);
    }
    // Compiled extern/any conversion (modes 40-41) re-tag a reference token.
    if mode == 40 || mode == 41 {
        return gc_convert_op(store, mode, x, scratch);
    }
    // Compiled array bulk ops (modes 50-56) mirror the interpreter's array
    // segment/fill/copy semantics against the store's object pool.
    if (50..=56).contains(&mode) {
        return array_bulk_op(store, instance, mode, x, y, z, nargs, scratch);
    }
    // A compiled ref.test/ref.cast match check (mode 60) runs the runtime
    // type-lattice test against the store.
    if mode == 60 {
        return ref_cast_op(store, instance, x, y, z, scratch);
    }
    // A compiled v128 register op (mode 70) reproduces the interpreter's
    // `simd_exec` over operands spilled to the caller-owned scratch.
    if mode == 70 {
        return simd_op(store, x, y, scratch);
    }
    // Compiled v128 memory/shuffle ops (modes 71-74) mirror the interpreter's
    // vector loads/stores over a bounds-checked effective address.
    if (71..=74).contains(&mode) {
        return v128_mem(store, mode, x, y, z, scratch);
    }
    // Table reads/writes (modes 3/4), the bulk-memory ops (modes 5-8), and
    // the table bulk ops (modes 9-14) resolve cells/segments directly.
    if (3..=14).contains(&mode) {
        if mode == 3 || mode == 4 {
            return table_op(store, instance, mode, x, y, z, scratch);
        }
        if mode <= 8 {
            return bulk_op(store, instance, mode, x, y, scratch);
        }
        return table_bulk_op(store, instance, mode, x, y, scratch);
    }
    // Resolve the target and the declared type with immutable reads only,
    // dropping every borrow before the store is mutated below.
    let caller = match store.instances.get(instance as usize) {
        Some(instance) => instance,
        None => return code_of(Trap::UnknownFunction),
    };
    let declared = match mode {
        0 => {
            let Some(ty) = store.func_type(instance as usize, x as usize) else {
                return code_of(Trap::UnknownFunction);
            };
            let Some(target) = caller.funcs.get(x as usize).copied() else {
                return code_of(Trap::UnknownFunction);
            };
            (ty, target)
        }
        1 => {
            let Some(ty) = caller.module.func_at_cloned(z as u32) else {
                return code_of(Trap::UnknownFunction);
            };
            let Some(cell) = caller.tables.get(x as usize).copied() else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            let Some(table) = store.tables.get(cell) else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            let Some(&entry) = table.elements.get(y as usize) else {
                return code_of(Trap::UndefinedElement);
            };
            let RefValue::Func(address) = entry else {
                return code_of(Trap::UninitializedElement);
            };
            let Some(target) = store
                .instances
                .get(address.instance)
                .and_then(|i| i.funcs.get(address.index))
                .copied()
            else {
                return code_of(Trap::UnknownFunction);
            };
            if !store.func_type_matches(&caller.module, z as u32, target) {
                return code_of(Trap::IndirectCallTypeMismatch);
            }
            (ty, target)
        }
        _ => {
            // `call_ref`: `x` is a function token (mode 2; anything else is
            // an internal error, not a wasm trap). A null reference is the
            // null-function-reference trap.
            let Some(value) = crate::values::token_to_ref(x) else {
                return crate::compile::TRAP_PENDING_ERROR;
            };
            let Value::Ref(RefValue::Func(address)) = value else {
                return code_of(Trap::NullFunctionReference);
            };
            let Some(ty) = caller.module.func_at_cloned(z as u32) else {
                return code_of(Trap::UnknownFunction);
            };
            let Some(target) = store
                .instances
                .get(address.instance)
                .and_then(|i| i.funcs.get(address.index))
                .copied()
            else {
                return code_of(Trap::UnknownFunction);
            };
            if !store.func_type_matches(&caller.module, z as u32, target) {
                return code_of(Trap::IndirectCallTypeMismatch);
            }
            (ty, target)
        }
    };
    let ty = declared.0;
    if ty.params.len() as u64 != nargs {
        store.set_pending_error(ExecFail::Unsupported("compiled call arity"));
        return crate::compile::TRAP_PENDING_ERROR;
    }
    // Native compiled-to-compiled re-entry: when the callee is itself a
    // compiled module-defined function and we are inside the native depth
    // budget, run it natively — each native level carries its own
    // descriptor/global/scratch buffers. Deeper chains (or interpreted or
    // host callees) fall through to the interpreter path below, which
    // preserves the interpreter's exhaustion semantics and bounds the native
    // stack.
    let native = match declared.1 {
        FuncTarget::Owned {
            instance: own,
            defined,
        } if !store.compile_off && store.native_depth < NATIVE_CALL_DEPTH => store
            .instances
            .get(own)
            .and_then(|inst| inst.compiled.get(defined))
            .and_then(|entry| entry.as_ref())
            .map(|func| (own, func as *const crate::compile::CompiledFunc)),
        _ => None,
    };
    if let Some((own, native_ptr)) = native {
        store.native_depth += 1;
        // SAFETY: the native entry and its buffers are handled inside
        // `run_native_callee` (callee-owned buffers stay alive for the call;
        // `store` stays uniquely borrowed by this whole helper invocation).
        let code = unsafe {
            run_native_callee(
                store as *mut Store,
                native_ptr,
                own,
                NativeCall {
                    argc: ty.params.len(),
                    nout: ty.results.len(),
                    args_out: scratch,
                    caller_instance: instance as usize,
                    caller_mems: mems,
                },
            )
        };
        store.native_depth -= 1;
        return code;
    }
    // Decode the argument slots (numeric values, function tokens for
    // function-reference parameters, or a v128 as two words) in parameter
    // order.
    let mut args = Vec::with_capacity(ty.params.len());
    let mut word = 0usize;
    for param in &ty.params {
        match param {
            ValType::V128 => {
                let lo = unsafe { *scratch.add(word) };
                let hi = unsafe { *scratch.add(word + 1) };
                args.push(Value::V128(u128::from(lo) | (u128::from(hi) << 64)));
                word += 2;
            }
            _ => {
                let slot = unsafe { *scratch.add(word) };
                word += 1;
                match value_from_call_slot(*param, slot) {
                    Some(value) => args.push(value),
                    None => {
                        store.set_pending_error(ExecFail::Unsupported(
                            "unsupported value across a compiled call",
                        ));
                        return crate::compile::TRAP_PENDING_ERROR;
                    }
                }
            }
        }
    }
    // Run the callee through the interpreter (compile off for the whole
    // subtree): identical semantics, bounded native stack.
    let previous = store.compile_off;
    store.compile_off = true;
    let outcome = store.run_target(declared.1, &args);
    store.compile_off = previous;
    // The callee may have grown a memory (its `Vec` reallocated): refresh
    // the caller-owned descriptors so later compiled accesses see the new
    // data pointer/length. Growth persists across traps, so do this on every
    // return from the run.
    refresh_descriptors(store, instance, mems);
    match outcome {
        Ok(results) => {
            let mut word = 0usize;
            for result in results.iter() {
                match result {
                    Value::V128(bits) => {
                        unsafe {
                            *scratch.add(word) = *bits as u64;
                            *scratch.add(word + 1) = (*bits >> 64) as u64;
                        }
                        word += 2;
                    }
                    _ => {
                        let Some(slot) = value_to_call_slot(*result) else {
                            store.set_pending_error(ExecFail::Unsupported(
                                "unsupported result across a compiled call",
                            ));
                            return crate::compile::TRAP_PENDING_ERROR;
                        };
                        unsafe { *scratch.add(word) = slot };
                        word += 1;
                    }
                }
            }
            crate::compile::TRAP_NONE
        }
        Err(ExecFail::Trap(trap)) => code_of(trap),
        Err(error) => {
            store.set_pending_error(error);
            crate::compile::TRAP_PENDING_ERROR
        }
    }
}

/// The caller-side buffers a native compiled re-entry (Wave 3) hands to the
/// callee: the argument slots the caller already spilled (also where results
/// land), plus the caller's instance index and descriptor pointer so the
/// callee's memory growth can be refreshed back after the call.
#[cfg(feature = "compile")]
struct NativeCall {
    argc: usize,
    nout: usize,
    args_out: *mut u64,
    caller_instance: usize,
    caller_mems: *mut u64,
}

/// Run a compiled callee natively from the call helper (Wave 3 direct
/// compiled-to-compiled re-entry). The callee's arguments already sit in the
/// caller's scratch at `call.args_out`; the callee writes its results back
/// there and gets fresh caller-owned buffers for its own memories
/// (descriptors), used globals, and internal-call scratch. Its global
/// mutations are flushed back to the store's cells on return (writes persist
/// across traps), and the caller's memory descriptors are refreshed in case
/// the callee grew a memory. Returns the callee entry's trap code.
///
/// # Safety
///
/// `store` stays valid and uniquely borrowed for the whole call;
/// `native_ptr` points at a compiled entry kept alive by its instance;
/// `call.args_out`/`call.caller_mems` are the caller's buffers, alive for the
/// call; the buffers allocated here stay alive until the native call returns.
#[cfg(feature = "compile")]
unsafe fn run_native_callee(
    store: *mut Store,
    native_ptr: *const crate::compile::CompiledFunc,
    own: usize,
    call: NativeCall,
) -> i32 {
    let func = unsafe { &*native_ptr };
    let store_ref = unsafe { &mut *store };
    // The callee's memory descriptors, from its own instance's cells.
    let memories = store_ref.instances[own].memories.clone();
    let mut descriptors = Vec::with_capacity(2 * memories.len());
    for cell in &memories {
        match store_ref.memories.get(*cell) {
            Some(memory) => {
                descriptors.push(memory.bytes.as_ptr() as u64);
                descriptors.push(memory.bytes.len() as u64);
            }
            None => {
                descriptors.push(0);
                descriptors.push(0);
            }
        }
    }
    // The callee's used globals ride a caller-owned buffer seeded from its
    // cells (word layout per `seed_globals`), flushed back after the call.
    let used = func.used_globals().to_vec();
    let cells: Vec<usize> = used
        .iter()
        .map(|index| store_ref.instances[own].globals[*index as usize])
        .collect();
    let mut gvals = seed_globals(store_ref, &cells);
    let callee_scratch = vec![0u64; crate::compile::SCRATCH_SLOTS];
    let runtime = crate::compile::CompiledRuntime {
        store: store as u64,
        instance: own as u64,
        grow: memory_grow_helper as *const () as usize as u64,
        call: wasm_call_helper as *const () as usize as u64,
        scratch: callee_scratch.as_ptr() as u64,
    };
    // SAFETY: enforced by `CompiledFunc::call_raw`'s contract (the buffers
    // above stay alive for the call).
    let code = unsafe {
        func.call_raw(
            call.args_out,
            call.argc as u64,
            descriptors.as_ptr(),
            descriptors.len() as u64,
            gvals.as_mut_ptr(),
            call.args_out,
            call.nout as u64,
            runtime,
        )
    };
    // Flush the callee's global writes back to its cells (survive traps) and
    // refresh the caller's descriptors (the callee may have grown a memory
    // shared with the caller, or any memory the caller later touches).
    let store_ref = unsafe { &mut *store };
    flush_globals(store_ref, &cells, &gvals);
    refresh_descriptors(store_ref, call.caller_instance as u64, call.caller_mems);
    code
}

/// A compiled `table.get` (mode 3) or `table.set` (mode 4): resolve the
/// instance's table `x`, bounds-check slot `y`, and read/write the element.
/// A `table.get` writes the element's token to `scratch[0]`.
#[cfg(feature = "compile")]
fn table_op(
    store: &mut Store,
    instance: u64,
    mode: u64,
    x: u64,
    y: u64,
    z: u64,
    scratch: *mut u64,
) -> i32 {
    let code_of = crate::compile::code_of_trap;
    let Some(cell) = store
        .instances
        .get(instance as usize)
        .and_then(|inst| inst.tables.get(x as usize))
        .copied()
    else {
        return code_of(Trap::OutOfBoundsTableAccess);
    };
    let Some(len) = store.tables.get(cell).map(|table| table.elements.len()) else {
        return code_of(Trap::OutOfBoundsTableAccess);
    };
    if y as usize >= len {
        return code_of(Trap::OutOfBoundsTableAccess);
    }
    if mode == 3 {
        let entry = store.tables[cell].elements[y as usize];
        let Some(token) = crate::values::ref_to_token(Value::Ref(entry)) else {
            store.set_pending_error(ExecFail::Unsupported(
                "unsupported table element across a compiled table.get",
            ));
            return crate::compile::TRAP_PENDING_ERROR;
        };
        unsafe {
            *scratch = token;
        }
        crate::compile::TRAP_NONE
    } else {
        let Some(value) = crate::values::token_to_ref(z) else {
            store.set_pending_error(ExecFail::Unsupported(
                "unsupported table element across a compiled table.set",
            ));
            return crate::compile::TRAP_PENDING_ERROR;
        };
        let Value::Ref(reference) = value else {
            return crate::compile::TRAP_PENDING_ERROR;
        };
        store.tables[cell].elements[y as usize] = reference;
        crate::compile::TRAP_NONE
    }
}

/// A compiled bulk-memory op (runtime helper modes 5-8). The operand slots
/// (addresses/offsets/length, each a u64) ride `scratch[0..3]`:
/// `memory.copy` (5, `x`=dst memory, `y`=src memory), `memory.fill` (6,
/// `x`=memory, value in slot 1), `memory.init` (7, `x`=data index, `y`=
/// memory), `data.drop` (8, `x`=data index). Bounds failures are
/// `OutOfBoundsMemoryAccess`, mirroring the interpreter exactly (overlapping
/// copies move through a temporary; a dropped data segment is empty).
#[cfg(feature = "compile")]
fn bulk_op(store: &mut Store, instance: u64, mode: u64, x: u64, y: u64, scratch: *mut u64) -> i32 {
    let code_of = crate::compile::code_of_trap;
    let Some(inst) = store.instances.get(instance as usize) else {
        return code_of(Trap::UnknownFunction);
    };
    let slot = |i: usize| unsafe { *scratch.add(i) };
    match mode {
        5 => {
            // memory.copy: dst address, src address, length.
            let Some(&dst_cell) = inst.memories.get(x as usize) else {
                return code_of(Trap::OutOfBoundsMemoryAccess);
            };
            let Some(&src_cell) = inst.memories.get(y as usize) else {
                return code_of(Trap::OutOfBoundsMemoryAccess);
            };
            let (dst, src, len) = (slot(0), slot(1), slot(2));
            let Some(dst_size) = store.memories.get(dst_cell).map(|m| m.bytes.len() as u64) else {
                return code_of(Trap::OutOfBoundsMemoryAccess);
            };
            let Some(src_size) = store.memories.get(src_cell).map(|m| m.bytes.len() as u64) else {
                return code_of(Trap::OutOfBoundsMemoryAccess);
            };
            if dst.checked_add(len).is_none_or(|end| end > dst_size)
                || src.checked_add(len).is_none_or(|end| end > src_size)
            {
                return code_of(Trap::OutOfBoundsMemoryAccess);
            }
            let (dst, src, len) = (dst as usize, src as usize, len as usize);
            if dst_cell == src_cell {
                let bytes = &mut store.memories[dst_cell].bytes;
                bytes.copy_within(src..src + len, dst);
            } else {
                let source = store.memories[src_cell].bytes[src..src + len].to_vec();
                store.memories[dst_cell].bytes[dst..dst + len].copy_from_slice(&source);
            }
            crate::compile::TRAP_NONE
        }
        6 => {
            // memory.fill: dst address, byte value, length.
            let Some(&cell) = inst.memories.get(x as usize) else {
                return code_of(Trap::OutOfBoundsMemoryAccess);
            };
            let Some(memory) = store.memories.get_mut(cell) else {
                return code_of(Trap::OutOfBoundsMemoryAccess);
            };
            let (dst, len) = (slot(0) as usize, slot(2) as usize);
            if dst
                .checked_add(len)
                .is_none_or(|end| end > memory.bytes.len())
            {
                return code_of(Trap::OutOfBoundsMemoryAccess);
            }
            memory.bytes[dst..dst + len].fill(slot(1) as u8);
            crate::compile::TRAP_NONE
        }
        7 => {
            // memory.init: dst address, data offset, length.
            let Some(&cell) = inst.memories.get(y as usize) else {
                return code_of(Trap::OutOfBoundsMemoryAccess);
            };
            let Some(memory) = store.memories.get(cell) else {
                return code_of(Trap::OutOfBoundsMemoryAccess);
            };
            let dst = slot(0) as usize;
            let src = slot(1) as usize;
            let len = slot(2) as usize;
            let segment = inst
                .data_segments
                .get(x as usize)
                .and_then(Option::as_deref)
                .unwrap_or(&[]);
            if dst
                .checked_add(len)
                .is_none_or(|end| end > memory.bytes.len())
                || src.checked_add(len).is_none_or(|end| end > segment.len())
            {
                return code_of(Trap::OutOfBoundsMemoryAccess);
            }
            let bytes = &mut store.memories[cell].bytes;
            bytes[dst..dst + len].copy_from_slice(&segment[src..src + len]);
            crate::compile::TRAP_NONE
        }
        _ => {
            // data.drop: the segment becomes empty (a later init reads none).
            let Some(segment) = store.instances[instance as usize]
                .data_segments
                .get_mut(x as usize)
            else {
                return code_of(Trap::UnknownFunction);
            };
            *segment = None;
            crate::compile::TRAP_NONE
        }
    }
}

/// A compiled table bulk op (runtime helper modes 9-14). The operand slots
/// (table addresses, lengths, and the reference token where one exists, each
/// a u64) ride `scratch[0..3]`: `table.size` (9, `x`=table, size written to
/// `scratch[0]`), `table.grow` (10, `x`=table, delta at slot 0 and the init
/// token at slot 1, old length or all-ones written to slot 0), `table.fill`
/// (11, `x`=table, dst/value/len at slots 0-2), `table.init` (12, `x`=elem
/// index, `y`=table, dst/src/len at slots 0-2), `table.copy` (13, `x`=dst
/// table, `y`=src table, dst/src/len at slots 0-2), and `elem.drop` (14,
/// `x`=elem index). Bounds failures are `OutOfBoundsTableAccess`, mirroring
/// the interpreter exactly (a dropped element segment is empty, overlapping
/// same-table copies move through a temporary, and growth caps follow the
/// table's address type). The `size`/`grow` result is the full u64 scalar
/// (an all-ones failure), and the compiled body loads it at its own width.
#[cfg(feature = "compile")]
fn table_bulk_op(
    store: &mut Store,
    instance: u64,
    mode: u64,
    x: u64,
    y: u64,
    scratch: *mut u64,
) -> i32 {
    let code_of = crate::compile::code_of_trap;
    match mode {
        9 => {
            // table.size: the current length as the table's address width.
            let Some(cell) = table_cell_of(store, instance, x) else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            let Some(size) = table_cell_len(store, cell) else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            unsafe {
                *scratch = size;
            }
            crate::compile::TRAP_NONE
        }
        10 => {
            // table.grow: grow by `slot(0)` slots filled with the reference
            // decoded from `slot(1)`; the old length, or all-ones (an i32/i64
            // -1 at the table's width) when the growth would exceed the
            // limits.
            let Some(cell) = table_cell_of(store, instance, x) else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            let delta = unsafe { *scratch };
            let token = unsafe { *scratch.add(1) };
            let Some(value) = crate::values::token_to_ref(token) else {
                store.set_pending_error(ExecFail::Unsupported(
                    "unsupported table element across a compiled table.grow",
                ));
                return crate::compile::TRAP_PENDING_ERROR;
            };
            let Value::Ref(init) = value else {
                return crate::compile::TRAP_PENDING_ERROR;
            };
            let table64 = table_cell_is64(store, cell);
            let old = store.tables[cell].grow(delta, init, table64);
            unsafe {
                *scratch = old.unwrap_or(u64::MAX);
            }
            crate::compile::TRAP_NONE
        }
        11 => {
            // table.fill: fill `[dst, dst+len)` with the reference token.
            let Some(cell) = table_cell_of(store, instance, x) else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            let Some(size) = table_cell_len(store, cell) else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            let dst = unsafe { *scratch };
            let len = unsafe { *scratch.add(2) };
            if dst.checked_add(len).is_none_or(|end| end > size) {
                return code_of(Trap::OutOfBoundsTableAccess);
            }
            let Some(value) = crate::values::token_to_ref(unsafe { *scratch.add(1) }) else {
                store.set_pending_error(ExecFail::Unsupported(
                    "unsupported table element across a compiled table.fill",
                ));
                return crate::compile::TRAP_PENDING_ERROR;
            };
            let Value::Ref(fill) = value else {
                return crate::compile::TRAP_PENDING_ERROR;
            };
            let (dst, len) = (dst as usize, len as usize);
            store.tables[cell].elements[dst..dst + len].fill(fill);
            crate::compile::TRAP_NONE
        }
        12 => {
            // table.init: copy `[src, src+len)` of element segment `x` into
            // table `y` at `[dst, dst+len)`; a dropped segment is empty.
            let Some(cell) = table_cell_of(store, instance, y) else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            let Some(size) = table_cell_len(store, cell) else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            let (dst, src, len) = (unsafe { *scratch }, unsafe { *scratch.add(1) }, unsafe {
                *scratch.add(2)
            });
            let Some(segment) = store.instances[instance as usize]
                .element_segments
                .get(x as usize)
            else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            let source = segment.as_deref().unwrap_or(&[]);
            if dst.checked_add(len).is_none_or(|end| end > size)
                || src
                    .checked_add(len)
                    .is_none_or(|end| end > source.len() as u64)
            {
                return code_of(Trap::OutOfBoundsTableAccess);
            }
            let (dst, src, len) = (dst as usize, src as usize, len as usize);
            let elements = &mut store.tables[cell].elements;
            elements[dst..dst + len].copy_from_slice(&source[src..src + len]);
            crate::compile::TRAP_NONE
        }
        13 => {
            // table.copy: `[src, src+len)` of the `y` table into `[dst,
            // dst+len)` of the `x` table; same-cell copies behave like a
            // memmove, different cells snapshot the source first.
            let Some(dst_cell) = table_cell_of(store, instance, x) else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            let Some(src_cell) = table_cell_of(store, instance, y) else {
                return code_of(Trap::OutOfBoundsTableAccess);
            };
            let (dst_size, src_size) = match (
                table_cell_len(store, dst_cell),
                table_cell_len(store, src_cell),
            ) {
                (Some(dst), Some(src)) => (dst, src),
                _ => return code_of(Trap::OutOfBoundsTableAccess),
            };
            let (dst, src, len) = (unsafe { *scratch }, unsafe { *scratch.add(1) }, unsafe {
                *scratch.add(2)
            });
            if dst.checked_add(len).is_none_or(|end| end > dst_size)
                || src.checked_add(len).is_none_or(|end| end > src_size)
            {
                return code_of(Trap::OutOfBoundsTableAccess);
            }
            let (dst, src, len) = (dst as usize, src as usize, len as usize);
            if dst_cell == src_cell {
                let elements = &mut store.tables[dst_cell].elements;
                elements.copy_within(src..src + len, dst);
            } else {
                let source = store.tables[src_cell].elements[src..src + len].to_vec();
                store.tables[dst_cell].elements[dst..dst + len].copy_from_slice(&source);
            }
            crate::compile::TRAP_NONE
        }
        _ => {
            // elem.drop: the segment becomes empty (a later init reads none).
            let Some(segment) = store.instances[instance as usize]
                .element_segments
                .get_mut(x as usize)
            else {
                return code_of(Trap::UnknownFunction);
            };
            *segment = None;
            crate::compile::TRAP_NONE
        }
    }
}

/// The store cell of `instance`'s table index-space entry `table`.
#[cfg(feature = "compile")]
fn table_cell_of(store: &Store, instance: u64, table: u64) -> Option<usize> {
    store
        .instances
        .get(instance as usize)
        .and_then(|inst| inst.tables.get(table as usize))
        .copied()
}

/// A table cell's current length, or `None` when the cell is gone.
#[cfg(feature = "compile")]
fn table_cell_len(store: &Store, cell: usize) -> Option<u64> {
    store
        .tables
        .get(cell)
        .map(|table| table.elements.len() as u64)
}

/// A table cell's `table64` flag (its address operands and `table.size`/
/// `table.grow` values are i64), used to cap growth exactly like the
/// interpreter's cell-level `grow`.
#[cfg(feature = "compile")]
fn table_cell_is64(store: &Store, cell: usize) -> bool {
    store
        .table_types
        .get(cell)
        .map(|table_type| table_type.table64)
        .unwrap_or(false)
}

/// A compiled `throw` of tag index-space entry `x` (runtime helper mode 15):
/// decode the payload from `scratch[0..nargs)` in parameter order and park an
/// in-flight exception on the store as a pending error, exactly like the
/// interpreter's `throw` (the exception escapes — no compiled body can
/// contain a `try_table` catch, so there is nothing to catch inside the
/// native path and the entry drains it into [`ExecFail::Exception`]).
#[cfg(feature = "compile")]
fn throw_op(store: &mut Store, instance: u64, x: u64, nargs: u64, scratch: *mut u64) -> i32 {
    let Some(cell) = store
        .instances
        .get(instance as usize)
        .and_then(|inst| inst.tags.get(x as usize))
        .copied()
    else {
        return crate::compile::code_of_trap(Trap::UnknownFunction);
    };
    let Some(tag) = store.tags.get(cell) else {
        return crate::compile::code_of_trap(Trap::UnknownFunction);
    };
    let params = tag.ty.params.clone();
    if params.len() as u64 != nargs {
        store.set_pending_error(ExecFail::Unsupported("compiled throw arity"));
        return crate::compile::TRAP_PENDING_ERROR;
    }
    let mut args = Vec::with_capacity(params.len());
    for (i, param) in params.iter().enumerate() {
        let slot = unsafe { *scratch.add(i) };
        let Some(value) = value_from_call_slot(*param, slot) else {
            store.set_pending_error(ExecFail::Unsupported(
                "unsupported payload across a compiled throw",
            ));
            return crate::compile::TRAP_PENDING_ERROR;
        };
        args.push(value);
    }
    let exn = store.new_exception(cell, args);
    store.set_pending_error(ExecFail::Exception(exn));
    crate::compile::TRAP_PENDING_ERROR
}

/// A compiled `throw_ref` (runtime helper mode 16): re-raise the exception
/// whose store-pool id is `x`, exactly like the interpreter's `throw_ref` on a
/// non-null `exnref` — the exception is already in the pool, so this only
/// parks it as a pending error for the entry to drain.
#[cfg(feature = "compile")]
fn rethrow_op(store: &mut Store, x: u64) -> i32 {
    store.set_pending_error(ExecFail::Exception(x as usize));
    crate::compile::TRAP_PENDING_ERROR
}

/// A compiled GC aggregate op (runtime helper modes 30-38), mirroring the
/// interpreter's struct/array semantics exactly. Operands ride the caller-
/// owned scratch as u64 slots (reference values as tokens); a single result
/// value is written back to `scratch[0]`. Modes: `30` = `struct.new` (ty=`x`,
/// field values at `scratch[0..nargs)`), `31` = `struct.new_default` (ty=`x`),
/// `32` = `struct.get`/`_s`/`_u` (field=`x`, read mode=`y`, object token at
/// `scratch[0]`, result to `scratch[0]`), `33` = `struct.set` (field=`x`,
/// object at `scratch[0]`, value at `scratch[1]`), `34` = `array.new` (ty=`x`,
/// value at `scratch[0]`, length at `scratch[1]`), `35` = `array.new_default`
/// (ty=`x`, length at `scratch[0]`), `36` = `array.get`/`_s`/`_u` (read mode=
/// `y`, object at `scratch[0]`, index at `scratch[1]`), `37` = `array.set`
/// (object at `scratch[0]`, index at `scratch[1]`, value at `scratch[2]`),
/// `38` = `array.len` (object at `scratch[0]`, length to `scratch[0]`).
/// Newly allocated struct/array tokens land in `scratch[0]`.
#[cfg(feature = "compile")]
#[allow(clippy::too_many_arguments)]
fn gc_op(
    store: &mut Store,
    instance: u64,
    mode: u64,
    x: u64,
    y: u64,
    _z: u64,
    nargs: u64,
    scratch: *mut u64,
) -> i32 {
    let code_of = crate::compile::code_of_trap;
    let slot = |i: usize| unsafe { *scratch.add(i) };
    let write = |i: usize, value: u64| unsafe { *scratch.add(i) = value };
    let instance = instance as usize;
    match mode {
        30 | 31 => {
            // struct.new / struct.new_default.
            let Some(storages) = gc_type_struct_fields(store, instance, x as u32) else {
                return code_of(Trap::UnknownFunction);
            };
            if mode == 30 && storages.len() as u64 != nargs {
                return gc_unsupported(store, "compiled struct.new arity");
            }
            let mut cells = Vec::with_capacity(storages.len());
            for (i, storage) in storages.into_iter().enumerate() {
                let cell = if mode == 31 {
                    match default_storage(storage) {
                        Ok(value) => value,
                        Err(_) => return gc_unsupported(store, "non-defaultable struct field"),
                    }
                } else {
                    let Some(value) = storage_from_slot(storage, slot(i)) else {
                        return gc_unsupported(store, "unsupported struct.new field");
                    };
                    match wrap_cell(storage, value) {
                        Ok(value) => value,
                        Err(_) => return gc_unsupported(store, "packed struct field write"),
                    }
                };
                cells.push(cell);
            }
            let id = alloc_struct(store, instance, x as u32, cells);
            match crate::values::struct_ref_token(id) {
                Some(token) => {
                    write(0, token);
                    crate::compile::TRAP_NONE
                }
                None => gc_unsupported(store, "struct pool id overflow"),
            }
        }
        32 => {
            // struct.get[_s|_u]: field `x`, read mode from `y`.
            let Some(id) = struct_object_of(slot(0)) else {
                return gc_object_error(store, code_of, slot(0), Trap::NullStructReference);
            };
            let out = match gc_read_struct(store, id, x as usize, y) {
                Ok(out) => out,
                Err(fail) => return fail,
            };
            match value_to_call_slot(out) {
                Some(bits) => {
                    write(0, bits);
                    crate::compile::TRAP_NONE
                }
                None => gc_unsupported(store, "unsupported struct.get result"),
            }
        }
        33 => {
            // struct.set: field `x`, object at slot 0, value at slot 1.
            let Some(id) = struct_object_of(slot(0)) else {
                return gc_object_error(store, code_of, slot(0), Trap::NullStructReference);
            };
            let Some(fields) = gc_object_struct_fields(store, id) else {
                return gc_unsupported(store, "struct object layout");
            };
            let Some(&storage) = fields.get(x as usize) else {
                return gc_unsupported(store, "struct field out of range");
            };
            let Some(value) = storage_from_slot(storage, slot(1)) else {
                return gc_unsupported(store, "unsupported struct.set value");
            };
            let cell = match wrap_cell(storage, value) {
                Ok(cell) => cell,
                Err(_) => return gc_unsupported(store, "packed struct field write"),
            };
            match &mut store.objects[id].data {
                GcData::Struct(cells) => {
                    cells[x as usize] = cell;
                    crate::compile::TRAP_NONE
                }
                _ => gc_unsupported(store, "struct.set of non-struct"),
            }
        }
        34 | 35 => {
            // array.new / array.new_default: ty=`x`, value+length or length.
            let Some(storage) = gc_type_array_storage(store, instance, x as u32) else {
                return code_of(Trap::UnknownFunction);
            };
            let len_slot = if mode == 34 { slot(1) } else { slot(0) };
            let cell = if mode == 34 {
                let Some(value) = storage_from_slot(storage, slot(0)) else {
                    return gc_unsupported(store, "unsupported array.new value");
                };
                match wrap_cell(storage, value) {
                    Ok(cell) => cell,
                    Err(_) => return gc_unsupported(store, "packed array element write"),
                }
            } else {
                match default_storage(storage) {
                    Ok(value) => value,
                    Err(_) => return gc_unsupported(store, "non-defaultable array element"),
                }
            };
            let len = len_slot as u32 as i32;
            match alloc_array_filled(store, instance, x as u32, len, cell) {
                Ok(id) => match crate::values::array_ref_token(id) {
                    Some(token) => {
                        write(0, token);
                        crate::compile::TRAP_NONE
                    }
                    None => gc_unsupported(store, "array pool id overflow"),
                },
                Err(ExecFail::Trap(trap)) => code_of(trap),
                Err(_) => gc_unsupported(store, "array allocation"),
            }
        }
        36 => {
            // array.get[_s|_u]: object at slot 0, index at slot 1.
            let Some(id) = array_object_of(slot(0)) else {
                return gc_object_error(store, code_of, slot(0), Trap::NullArrayReference);
            };
            let index = slot(1) as u32 as usize;
            let out = match gc_read_array(store, id, index, y) {
                Ok(out) => out,
                Err(fail) => return fail,
            };
            match value_to_call_slot(out) {
                Some(bits) => {
                    write(0, bits);
                    crate::compile::TRAP_NONE
                }
                None => gc_unsupported(store, "unsupported array.get result"),
            }
        }
        37 => {
            // array.set: object at slot 0, index at slot 1, value at slot 2.
            let Some(id) = array_object_of(slot(0)) else {
                return gc_object_error(store, code_of, slot(0), Trap::NullArrayReference);
            };
            let index = slot(1) as u32 as usize;
            let Some(storage) = gc_array_storage(store, id).ok() else {
                return gc_unsupported(store, "array object layout");
            };
            let Some(value) = storage_from_slot(storage, slot(2)) else {
                return gc_unsupported(store, "unsupported array.set value");
            };
            let cell = match wrap_cell(storage, value) {
                Ok(cell) => cell,
                Err(_) => return gc_unsupported(store, "packed array element write"),
            };
            match &mut store.objects[id].data {
                GcData::Array(cells) => {
                    if index >= cells.len() {
                        return code_of(Trap::OutOfBoundsArrayAccess);
                    }
                    cells[index] = cell;
                    crate::compile::TRAP_NONE
                }
                _ => gc_unsupported(store, "array.set of non-array"),
            }
        }
        _ => {
            // array.len: object at slot 0 -> its length.
            let Some(id) = array_object_of(slot(0)) else {
                return gc_object_error(store, code_of, slot(0), Trap::NullArrayReference);
            };
            let len = match &store.objects[id].data {
                GcData::Array(cells) => cells.len() as u64,
                _ => return gc_unsupported(store, "array.len of non-array"),
            };
            write(0, len);
            crate::compile::TRAP_NONE
        }
    }
}

/// A compiled array bulk op (runtime helper modes 50-56), mirroring the
/// interpreter's array segment/fill/copy semantics exactly. Operands ride the
/// caller-owned scratch as u64 slots (array references as tokens, i32
/// offsets/lengths zero-extended). Modes: `50` = `array.new_fixed` (ty=`x`,
/// `nargs` elements at `scratch[0..nargs)`), `51` = `array.fill` (object at
/// `scratch[0]`, start `scratch[1]`, value `scratch[2]`, length `scratch[3]`),
/// `52` = `array.copy` (dst object `scratch[0]`, dst offset `scratch[1]`, src
/// object `scratch[2]`, src offset `scratch[3]`, length `scratch[4]`),
/// `53` = `array.new_data` (ty=`x`, data index=`y`, src `scratch[0]`, length
/// `scratch[1]`), `54` = `array.new_elem` (ty=`x`, elem index=`y`, src
/// `scratch[0]`, length `scratch[1]`), `55` = `array.init_data` (ty=`x`, data
/// index=`y`, object `scratch[0]`, dst offset `scratch[1]`, src `scratch[2]`,
/// length `scratch[3]`), `56` = `array.init_elem` (ty=`x`, elem index=`y`,
/// same slots as `55`). Newly allocated array tokens land in `scratch[0]`;
/// out-of-bounds ranges trap with the interpreter's trap kind per source
/// (array, memory for a data segment, table for an element segment).
#[cfg(feature = "compile")]
#[allow(clippy::too_many_arguments)]
fn array_bulk_op(
    store: &mut Store,
    instance: u64,
    mode: u64,
    x: u64,
    y: u64,
    _z: u64,
    nargs: u64,
    scratch: *mut u64,
) -> i32 {
    let code_of = crate::compile::code_of_trap;
    let slot = |i: usize| unsafe { *scratch.add(i) };
    let write = |i: usize, value: u64| unsafe { *scratch.add(i) = value };
    let instance = instance as usize;
    let array_id = |object: u64| -> Option<usize> { array_object_of(object) };
    match mode {
        50 => {
            // array.new_fixed: the elements arrive in slot order.
            let Some(storage) = gc_type_array_storage(store, instance, x as u32) else {
                return code_of(Trap::UnknownFunction);
            };
            let mut cells = Vec::with_capacity(nargs as usize);
            for i in 0..nargs as usize {
                let Some(value) = storage_from_slot(storage, slot(i)) else {
                    return gc_unsupported(store, "unsupported array.new_fixed element");
                };
                let cell = match wrap_cell(storage, value) {
                    Ok(cell) => cell,
                    Err(_) => return gc_unsupported(store, "packed array element write"),
                };
                cells.push(cell);
            }
            let id = alloc_array(store, instance, x as u32, cells);
            match crate::values::array_ref_token(id) {
                Some(token) => {
                    write(0, token);
                    crate::compile::TRAP_NONE
                }
                None => gc_unsupported(store, "array pool id overflow"),
            }
        }
        51 => {
            // array.fill: object, start, value, length.
            let Some(id) = array_id(slot(0)) else {
                return gc_object_error(store, code_of, slot(0), Trap::NullArrayReference);
            };
            let start = slot(1) as u32 as usize;
            let len = slot(3) as u32 as usize;
            let Some(storage) = gc_array_storage(store, id).ok() else {
                return gc_unsupported(store, "array object layout");
            };
            let Some(value) = storage_from_slot(storage, slot(2)) else {
                return gc_unsupported(store, "unsupported array.fill value");
            };
            let cell = match wrap_cell(storage, value) {
                Ok(cell) => cell,
                Err(_) => return gc_unsupported(store, "packed array element write"),
            };
            let cells = match &mut store.objects[id].data {
                GcData::Array(cells) => cells,
                _ => return gc_unsupported(store, "array.fill of non-array"),
            };
            let Some(end) = start.checked_add(len) else {
                return code_of(Trap::OutOfBoundsArrayAccess);
            };
            if end > cells.len() {
                return code_of(Trap::OutOfBoundsArrayAccess);
            }
            cells[start..end].fill(cell);
            crate::compile::TRAP_NONE
        }
        52 => {
            // array.copy: dst object, dst offset, src object, src offset, len.
            let Some(dst) = array_id(slot(0)) else {
                return gc_object_error(store, code_of, slot(0), Trap::NullArrayReference);
            };
            let Some(src) = array_id(slot(2)) else {
                return gc_object_error(store, code_of, slot(2), Trap::NullArrayReference);
            };
            let dst_off = slot(1) as u32 as usize;
            let src_off = slot(3) as u32 as usize;
            let n = slot(4) as u32 as usize;
            let (src_len, dst_len) = match (&store.objects[src].data, &store.objects[dst].data) {
                (GcData::Array(a), GcData::Array(b)) => (a.len(), b.len()),
                _ => return gc_unsupported(store, "array.copy of non-array"),
            };
            let Some(src_end) = src_off.checked_add(n) else {
                return code_of(Trap::OutOfBoundsArrayAccess);
            };
            let Some(dst_end) = dst_off.checked_add(n) else {
                return code_of(Trap::OutOfBoundsArrayAccess);
            };
            if src_end > src_len || dst_end > dst_len {
                return code_of(Trap::OutOfBoundsArrayAccess);
            }
            let values = match &store.objects[src].data {
                GcData::Array(cells) => cells[src_off..src_end].to_vec(),
                _ => return gc_unsupported(store, "array.copy of non-array"),
            };
            match &mut store.objects[dst].data {
                GcData::Array(cells) => cells[dst_off..dst_end].copy_from_slice(&values),
                _ => return gc_unsupported(store, "array.copy of non-array"),
            }
            crate::compile::TRAP_NONE
        }
        53 | 54 => {
            // array.new_data / array.new_elem: src and length, built from the
            // data/element segment.
            let Some(storage) = gc_type_array_storage(store, instance, x as u32) else {
                return code_of(Trap::UnknownFunction);
            };
            let src = slot(0) as u32 as u64;
            let n = slot(1) as u32 as u64;
            let mut cells = Vec::with_capacity(n as usize);
            if mode == 53 {
                let Some(width) = storage_width(storage) else {
                    return gc_unsupported(store, "array.new_data of references");
                };
                let bytes = match store.instances[instance].data_segments.get(y as usize) {
                    Some(segment) => segment.as_deref().unwrap_or(&[]),
                    None => return code_of(Trap::OutOfBoundsMemoryAccess),
                };
                let total = match n.checked_mul(width as u64) {
                    Some(total) => total,
                    None => return code_of(Trap::OutOfBoundsMemoryAccess),
                };
                let end = match src.checked_add(total) {
                    Some(end) => end,
                    None => return code_of(Trap::OutOfBoundsMemoryAccess),
                };
                if end > bytes.len() as u64 {
                    return code_of(Trap::OutOfBoundsMemoryAccess);
                }
                for i in 0..n {
                    match data_element(storage, bytes, (src + i * width as u64) as usize) {
                        Some(cell) => cells.push(cell),
                        None => return gc_unsupported(store, "array.new_data element read"),
                    }
                }
            } else {
                let items = match store.instances[instance].element_segments.get(y as usize) {
                    Some(segment) => segment.as_deref().unwrap_or(&[]),
                    None => return code_of(Trap::OutOfBoundsTableAccess),
                };
                let end = match src.checked_add(n) {
                    Some(end) => end,
                    None => return code_of(Trap::OutOfBoundsTableAccess),
                };
                if end > items.len() as u64 {
                    return code_of(Trap::OutOfBoundsTableAccess);
                }
                for i in 0..n {
                    cells.push(Value::Ref(items[(src + i) as usize]));
                }
            }
            let id = alloc_array(store, instance, x as u32, cells);
            match crate::values::array_ref_token(id) {
                Some(token) => {
                    write(0, token);
                    crate::compile::TRAP_NONE
                }
                None => gc_unsupported(store, "array pool id overflow"),
            }
        }
        55 | 56 => {
            // array.init_data / array.init_elem: dst object, dst offset, src,
            // length.
            let Some(id) = array_id(slot(0)) else {
                return gc_object_error(store, code_of, slot(0), Trap::NullArrayReference);
            };
            let dst_off = slot(1) as u32 as usize;
            let src = slot(2) as u32 as u64;
            let n = slot(3) as u32 as u64;
            let Some(storage) = gc_array_storage(store, id).ok() else {
                return gc_unsupported(store, "array object layout");
            };
            let len = match &store.objects[id].data {
                GcData::Array(cells) => cells.len(),
                _ => return gc_unsupported(store, "array.init of non-array"),
            };
            let Some(dst_end) = dst_off.checked_add(n as usize) else {
                return code_of(Trap::OutOfBoundsArrayAccess);
            };
            if dst_end > len {
                return code_of(Trap::OutOfBoundsArrayAccess);
            }
            let cells = match &mut store.objects[id].data {
                GcData::Array(cells) => cells,
                _ => return gc_unsupported(store, "array.init of non-array"),
            };
            if mode == 55 {
                let Some(width) = storage_width(storage) else {
                    return gc_unsupported(store, "array.init_data of references");
                };
                let bytes = match store.instances[instance].data_segments.get(y as usize) {
                    Some(segment) => segment.as_deref().unwrap_or(&[]),
                    None => return code_of(Trap::OutOfBoundsMemoryAccess),
                };
                let total = match n.checked_mul(width as u64) {
                    Some(total) => total,
                    None => return code_of(Trap::OutOfBoundsMemoryAccess),
                };
                let end = match src.checked_add(total) {
                    Some(end) => end,
                    None => return code_of(Trap::OutOfBoundsMemoryAccess),
                };
                if end > bytes.len() as u64 {
                    return code_of(Trap::OutOfBoundsMemoryAccess);
                }
                for i in 0..n {
                    let value =
                        match data_element(storage, bytes, (src + i * width as u64) as usize) {
                            Some(value) => value,
                            None => return gc_unsupported(store, "array.init_data element read"),
                        };
                    let cell = match wrap_cell(storage, value) {
                        Ok(cell) => cell,
                        Err(_) => return gc_unsupported(store, "packed array element write"),
                    };
                    cells[dst_off + i as usize] = cell;
                }
            } else {
                let items = match store.instances[instance].element_segments.get(y as usize) {
                    Some(segment) => segment.as_deref().unwrap_or(&[]),
                    None => return code_of(Trap::OutOfBoundsTableAccess),
                };
                let end = match src.checked_add(n) {
                    Some(end) => end,
                    None => return code_of(Trap::OutOfBoundsTableAccess),
                };
                if end > items.len() as u64 {
                    return code_of(Trap::OutOfBoundsTableAccess);
                }
                for i in 0..n {
                    cells[dst_off + i as usize] = Value::Ref(items[(src + i) as usize]);
                }
            }
            crate::compile::TRAP_NONE
        }
        _ => gc_unsupported(store, "bad array bulk mode"),
    }
}

/// The struct/array field storage types declared at type index `ty` of
/// `instance`'s module.
#[cfg(feature = "compile")]
fn gc_type_struct_fields(store: &Store, instance: usize, ty: u32) -> Option<Vec<StorageType>> {
    let module = &store.instances.get(instance)?.module;
    struct_field_types(module, ty).map(|fields| fields.iter().map(|field| field.ty).collect())
}

/// The array element storage declared at type index `ty` of `instance`'s
/// module.
#[cfg(feature = "compile")]
fn gc_type_array_storage(store: &Store, instance: usize, ty: u32) -> Option<StorageType> {
    let module = &store.instances.get(instance)?.module;
    array_element_type(module, ty).map(|field| field.ty)
}

/// The storage types of an existing struct object (from its own type).
#[cfg(feature = "compile")]
fn gc_object_struct_fields(store: &Store, object: usize) -> Option<Vec<StorageType>> {
    let gc_object = &store.objects.get(object)?;
    let module = &store.instances.get(gc_object.owner)?.module;
    struct_field_types(module, gc_object.ty)
        .map(|fields| fields.iter().map(|field| field.ty).collect())
}

/// The store-pool id when `token` is a struct object token, else `None`.
#[cfg(feature = "compile")]
fn struct_object_of(token: u64) -> Option<usize> {
    match crate::values::token_to_ref(token) {
        Some(Value::Ref(RefValue::Struct(id))) => Some(id),
        _ => None,
    }
}

/// The store-pool id when `token` is an array object token, else `None`.
#[cfg(feature = "compile")]
fn array_object_of(token: u64) -> Option<usize> {
    match crate::values::token_to_ref(token) {
        Some(Value::Ref(RefValue::Array(id))) => Some(id),
        _ => None,
    }
}

/// A `struct.get[_s|_u]` read of field `field` (read mode `read`) of the
/// struct object `id`. `Ok` carries the read value; a non-trap failure is
/// parked on the store and reported as the pending-error sentinel.
#[cfg(feature = "compile")]
fn gc_read_struct(store: &mut Store, id: usize, field: usize, read: u64) -> Result<Value, i32> {
    let storage = match gc_object_struct_fields(store, id) {
        Some(storage) => storage,
        None => return Err(gc_unsupported(store, "struct object layout")),
    };
    let Some(st) = storage.get(field).copied() else {
        return Err(gc_unsupported(store, "struct field out of range"));
    };
    let Some(&cell) = (match &store.objects[id].data {
        GcData::Struct(cells) => cells.get(field),
        _ => None,
    }) else {
        return Err(gc_unsupported(store, "struct.get of non-struct"));
    };
    let mode = match read {
        1 => GcRead::Signed,
        2 => GcRead::Unsigned,
        _ => GcRead::Plain,
    };
    match mode {
        GcRead::Plain => Ok(cell),
        GcRead::Signed => extend_cell(st, cell, true)
            .map_err(|_| gc_unsupported(store, "packed signed struct read")),
        GcRead::Unsigned => extend_cell(st, cell, false)
            .map_err(|_| gc_unsupported(store, "packed unsigned struct read")),
    }
}

/// An `array.get[_s|_u]` read of element `index` (read mode `read`) of the
/// array object `id`. `Ok` carries the read value; an index past the end is
/// the out-of-bounds-array trap and other failures are parked.
#[cfg(feature = "compile")]
fn gc_read_array(store: &mut Store, id: usize, index: usize, read: u64) -> Result<Value, i32> {
    let code_of = crate::compile::code_of_trap;
    let st = match gc_array_storage(store, id) {
        Ok(st) => st,
        Err(_) => return Err(gc_unsupported(store, "array object layout")),
    };
    let Some(&cell) = (match &store.objects[id].data {
        GcData::Array(cells) => cells.get(index),
        _ => None,
    }) else {
        return match &store.objects[id].data {
            GcData::Array(_) => Err(code_of(Trap::OutOfBoundsArrayAccess)),
            _ => Err(gc_unsupported(store, "array.get of non-array")),
        };
    };
    let mode = match read {
        1 => GcRead::Signed,
        2 => GcRead::Unsigned,
        _ => GcRead::Plain,
    };
    match mode {
        GcRead::Plain => Ok(cell),
        GcRead::Signed => extend_cell(st, cell, true)
            .map_err(|_| gc_unsupported(store, "packed signed array read")),
        GcRead::Unsigned => extend_cell(st, cell, false)
            .map_err(|_| gc_unsupported(store, "packed unsigned array read")),
    }
}

/// An object read/write hit an object that is not the expected struct/array
/// kind: a null operand is its null-reference trap; anything else is an
/// unsupported (parked) error.
#[cfg(feature = "compile")]
fn gc_object_error(
    store: &mut Store,
    code_of: fn(Trap) -> i32,
    token: u64,
    null_trap: Trap,
) -> i32 {
    match crate::values::token_to_ref(token) {
        Some(Value::Ref(RefValue::Null)) => code_of(null_trap),
        _ => gc_unsupported(store, "non-object GC operand"),
    }
}

/// A compiled `extern.convert_any` (mode 40) / `any.convert_extern` (mode
/// 41): convert between an internal `any` value and its `extern` box by
/// re-tagging the token, exactly like the interpreter's `wrap`/`unwrap_extern`
/// (a null passes through unchanged; host values ride the `any` token region,
/// so unboxing a host external works too). The result token lands in
/// `scratch[0]`.
#[cfg(feature = "compile")]
fn gc_convert_op(store: &mut Store, mode: u64, x: u64, scratch: *mut u64) -> i32 {
    let Some(value) = crate::values::token_to_ref(x) else {
        return gc_unsupported(store, "unencodable convert operand");
    };
    let token = match (mode, value) {
        (40, Value::Ref(reference)) => {
            if reference == RefValue::Null {
                Some(crate::values::REF_NULL_TOKEN)
            } else {
                match ExternInner::wrap(reference) {
                    Some(inner) => crate::values::ref_to_token(Value::Ref(RefValue::Extern(inner))),
                    None => {
                        return gc_unsupported(store, "extern.convert_any of unboxable reference");
                    }
                }
            }
        }
        (41, Value::Ref(reference)) => {
            if reference == RefValue::Null {
                Some(crate::values::REF_NULL_TOKEN)
            } else {
                let RefValue::Extern(inner) = reference else {
                    return gc_unsupported(store, "any.convert_extern of non-extern");
                };
                let internal = inner.into_ref();
                if matches!(internal, RefValue::Func(_) | RefValue::Exn(_)) {
                    return gc_unsupported(store, "any.convert_extern of an unboxable external");
                }
                crate::values::ref_to_token(Value::Ref(internal))
            }
        }
        _ => return gc_unsupported(store, "bad convert mode"),
    };
    match token {
        Some(token) => unsafe {
            *scratch = token;
        },
        None => return gc_unsupported(store, "unencodable convert result"),
    }
    crate::compile::TRAP_NONE
}

/// Park a non-trap error for the compiled entry to drain and return the
/// pending-error sentinel.
#[cfg(feature = "compile")]
fn gc_unsupported(store: &mut Store, what: &'static str) -> i32 {
    store.set_pending_error(ExecFail::Unsupported(what));
    crate::compile::TRAP_PENDING_ERROR
}

/// The declared type index of a function reference within its owner module,
/// when it resolves to one (host functions from outside a module's type space
/// are treated as abstract `func`, exactly like the interpreter).
#[cfg(feature = "compile")]
fn gc_func_type_index(store: &Store, addr: FuncAddr) -> Option<u32> {
    let instance = store.instances.get(addr.instance)?;
    let func_imports = instance
        .module
        .imports
        .iter()
        .filter(|import| matches!(import.desc, ImportDesc::Func(_)))
        .count();
    if addr.index < func_imports {
        instance
            .module
            .imports
            .iter()
            .filter_map(|import| match import.desc {
                ImportDesc::Func(ty) => Some(ty),
                _ => None,
            })
            .nth(addr.index)
    } else {
        instance
            .module
            .functions
            .get(addr.index - func_imports)
            .copied()
    }
}

/// Whether a runtime reference matches a target reftype (`nullable`, `heap`)
/// whose type indices live in `frame`'s module, mirroring the interpreter's
/// `Engine::ref_matches`: null matches only a nullable target; i31/host/
/// extern/exn and unresolved funcs match by their abstract heap; struct/array
/// objects and resolved funcs match by subtype (same-module) or by walking
/// the owner's supertype chain against the frame module's target type
/// (cross-module equivalence).
#[cfg(feature = "compile")]
fn ref_matches_target(
    store: &Store,
    frame: usize,
    reference: RefValue,
    nullable: bool,
    heap: HeapType,
) -> Option<bool> {
    if reference == RefValue::Null {
        return Some(nullable);
    }
    let concrete = match reference {
        RefValue::Func(addr) => match gc_func_type_index(store, addr) {
            Some(ty) => Some((addr.instance, ty)),
            None => return Some(runtime_abs_matches(HeapType::Func, heap)),
        },
        RefValue::Struct(id) => Some((store.objects.get(id)?.owner, store.objects.get(id)?.ty)),
        RefValue::Array(id) => Some((store.objects.get(id)?.owner, store.objects.get(id)?.ty)),
        RefValue::Host(_) => return Some(runtime_abs_matches(HeapType::Any, heap)),
        RefValue::I31(_) => return Some(runtime_abs_matches(HeapType::I31, heap)),
        RefValue::Extern(_) => return Some(runtime_abs_matches(HeapType::Extern, heap)),
        RefValue::Exn(_) => return Some(runtime_abs_matches(HeapType::Exn, heap)),
        RefValue::Null => return Some(nullable),
    };
    let (owner, ty) = concrete?;
    let module = &store.instances.get(owner)?.module;
    let composite = module.types.get(ty as usize)?.composite.clone();
    Some(match heap {
        HeapType::Type(target) => {
            let owner_module = &store.instances.get(owner)?.module;
            if owner == frame {
                owner_module.type_is_subtype(ty, target)
            } else {
                let frame_module = &store.instances.get(frame)?.module;
                let mut current = ty;
                loop {
                    if crate::module::type_indices_equivalent(
                        &frame_module.types,
                        &frame_module.rec_groups,
                        target,
                        &owner_module.types,
                        &owner_module.rec_groups,
                        current,
                    ) {
                        break true;
                    }
                    let Some(parent) = owner_module
                        .types
                        .get(current as usize)
                        .and_then(|sub| sub.supertypes.first())
                        .copied()
                    else {
                        break false;
                    };
                    current = parent;
                }
            }
        }
        abstract_target => {
            let kind = match &composite {
                CompositeType::Func(_) => HeapType::Func,
                CompositeType::Struct(_) => HeapType::Struct,
                CompositeType::Array(_) => HeapType::Array,
            };
            runtime_abs_matches(kind, abstract_target)
        }
    })
}

/// A compiled `ref.test`/`ref.cast` match check (runtime helper mode 60):
/// whether the reference token `x` matches the target reftype (`y` nullable,
/// `z` the target heap code — a type index or an abstract heap's negative
/// code). The 0/1 result lands in `scratch[0]`; a reference the token model
/// cannot decode parks an error.
#[cfg(feature = "compile")]
fn ref_cast_op(store: &mut Store, frame: u64, x: u64, y: u64, z: u64, scratch: *mut u64) -> i32 {
    let Some(value) = crate::values::token_to_ref(x) else {
        return gc_unsupported(store, "unencodable cast operand");
    };
    let Value::Ref(reference) = value else {
        return gc_unsupported(store, "non-reference cast operand");
    };
    let heap = if z as i64 >= 0 {
        HeapType::Type(z as u32)
    } else {
        match HeapType::from_s33(z as i64) {
            Some(heap) => heap,
            None => return gc_unsupported(store, "unknown cast target heap"),
        }
    };
    match ref_matches_target(store, frame as usize, reference, y != 0, heap) {
        Some(matches) => unsafe {
            *scratch = u64::from(matches);
        },
        None => return gc_unsupported(store, "cast target resolution"),
    }
    crate::compile::TRAP_NONE
}

/// Read the v128 (two u64 words, little-endian) at `scratch[word]`.
///
/// # Safety
///
/// `scratch` must point at `word + 2` writable u64 slots.
#[cfg(feature = "compile")]
unsafe fn scratch_read_v128(scratch: *mut u64, word: usize) -> u128 {
    // SAFETY: `scratch` points at `word + 2` readable u64 slots (enforced by
    // this helper's `# Safety` contract).
    unsafe {
        let lo = *scratch.add(word);
        let hi = *scratch.add(word + 1);
        u128::from(lo) | (u128::from(hi) << 64)
    }
}

/// Write the v128 `value` at `scratch[word]` (two u64 words, little-endian).
///
/// # Safety
///
/// `scratch` must point at `word + 2` writable u64 slots.
#[cfg(feature = "compile")]
unsafe fn scratch_write_v128(scratch: *mut u64, word: usize, value: u128) {
    // SAFETY: `scratch` points at `word + 2` writable u64 slots (enforced by
    // this helper's `# Safety` contract).
    unsafe {
        *scratch.add(word) = value as u64;
        *scratch.add(word + 1) = (value >> 64) as u64;
    }
}

/// A compiled v128 register op (runtime helper mode 70): reproduce the
/// interpreter's `simd_exec` dispatch over operand values read from the
/// caller-owned scratch (in interpreter pop order: a v128 operand is two u64
/// words, a scalar one word), writing the result back over `scratch[0]` — a
/// v128 as two words, a scalar as one. `sub` = x; the extract/replace lane
/// immediate = y. Relaxed ops dispatch first, exactly like the interpreter.
#[cfg(feature = "compile")]
fn simd_op(store: &mut Store, x: u64, y: u64, scratch: *mut u64) -> i32 {
    use crate::simd::{VecSig, lane_bits_to_value, scalar_to_lane_bits};
    let sub = x as u16;
    let lane = y as u8;
    let read = |word: usize| unsafe { scratch_read_v128(scratch, word) };
    let read_word = |index: usize| unsafe { *scratch.add(index) };
    let write = |word: usize, value: u128| unsafe { scratch_write_v128(scratch, word, value) };
    let write_word = |index: usize, bits: u64| unsafe { *scratch.add(index) = bits };
    let write_value = |value: Value| {
        let bits = match value {
            Value::I32(v) => v as u32 as u64,
            Value::I64(v) => v as u64,
            Value::F32(bits) => u64::from(bits),
            Value::F64(bits) => bits,
            // Scalar simd results are numeric; a V128/ref here is a bug.
            _ => 0,
        };
        write_word(0, bits);
    };
    // Relaxed SIMD ops take a fixed v128 operand count and one deterministic
    // behavior each (spec: any result in the allowed set). The operands ride
    // scratch slots in pop order (c, b, a for arity 3; b, a for 2; a for 1).
    if crate::simd::is_relaxed(sub) {
        let arity = crate::simd::relaxed_arity(sub);
        let out = match arity {
            1 => crate::simd::exec_relaxed(sub, read(0), None, None),
            2 => {
                let b = read(0);
                let a = read(2);
                crate::simd::exec_relaxed(sub, a, Some(b), None)
            }
            _ => {
                let c = read(0);
                let b = read(2);
                let a = read(4);
                crate::simd::exec_relaxed(sub, a, Some(b), Some(c))
            }
        };
        return match out {
            Some(out) => {
                write(0, out);
                crate::compile::TRAP_NONE
            }
            None => gc_unsupported(store, "relaxed simd opcode"),
        };
    }
    match crate::simd::sig(sub) {
        Some(VecSig::Not) => {
            write(0, crate::simd::exec_not(read(0)));
            crate::compile::TRAP_NONE
        }
        Some(VecSig::Unop) => match crate::simd::exec_unop(sub, read(0)) {
            Some(out) => {
                write(0, out);
                crate::compile::TRAP_NONE
            }
            None => gc_unsupported(store, "simd unop"),
        },
        Some(VecSig::Binop) => {
            let b = read(0);
            let a = read(2);
            match crate::simd::exec_binop(sub, a, b) {
                Some(out) => {
                    write(0, out);
                    crate::compile::TRAP_NONE
                }
                None => gc_unsupported(store, "simd binop"),
            }
        }
        Some(VecSig::Ternop) => {
            let c = read(0);
            let b = read(2);
            let a = read(4);
            write(0, crate::simd::exec_bitselect(a, b, c));
            crate::compile::TRAP_NONE
        }
        Some(VecSig::Shift) => {
            let count = read_word(0) as u32;
            let v = read(1);
            match crate::simd::exec_shift(sub, v, count) {
                Some(out) => {
                    write(0, out);
                    crate::compile::TRAP_NONE
                }
                None => gc_unsupported(store, "simd shift"),
            }
        }
        Some(VecSig::Splat(kind)) => {
            let scalar = lane_bits_to_value(read_word(0), kind);
            let Some(bits) = scalar_to_lane_bits(scalar, kind) else {
                return gc_unsupported(store, "simd splat operand");
            };
            match crate::simd::exec_splat(sub, bits) {
                Some(out) => {
                    write(0, out);
                    crate::compile::TRAP_NONE
                }
                None => gc_unsupported(store, "simd splat"),
            }
        }
        Some(VecSig::ExtractS(_) | VecSig::ExtractU(_) | VecSig::Extract(_)) => {
            match crate::simd::exec_extract(sub, read(0), lane as usize) {
                Some(value) => {
                    write_value(value);
                    crate::compile::TRAP_NONE
                }
                None => gc_unsupported(store, "simd extract"),
            }
        }
        Some(VecSig::Replace(kind)) => {
            let scalar = lane_bits_to_value(read_word(0), kind);
            let Some(bits) = scalar_to_lane_bits(scalar, kind) else {
                return gc_unsupported(store, "simd replace operand");
            };
            let v = read(1);
            match crate::simd::exec_replace(sub, v, lane as usize, bits) {
                Some(out) => {
                    write(0, out);
                    crate::compile::TRAP_NONE
                }
                None => gc_unsupported(store, "simd replace"),
            }
        }
        Some(VecSig::Test) => match crate::simd::exec_any_all_true(sub, read(0)) {
            Some(out) => {
                write_word(0, out as u32 as u64);
                crate::compile::TRAP_NONE
            }
            None => gc_unsupported(store, "simd all_true"),
        },
        Some(VecSig::Bitmask) => match crate::simd::exec_bitmask(sub, read(0)) {
            Some(out) => {
                write_word(0, out as u32 as u64);
                crate::compile::TRAP_NONE
            }
            None => gc_unsupported(store, "simd bitmask"),
        },
        None => gc_unsupported(store, "simd opcode"),
    }
}

/// A compiled v128 memory/shuffle op (runtime helper modes 71-74): shuffle
/// (71) composes a vector from the two vectors in scratch by the lane-index
/// bytes packed into `x`/`y` (eight per word); a `v128.load*` form (72)
/// materializes the vector from the `op` code in `x` over the `size` bytes at
/// `y` (the effective address, bounds-checked by the compiled `mem_ea`); a
/// lane load (73) reads `x` bytes at `z` into lane `y` of the vector in
/// scratch; a lane store (74) writes lane `y`'s `x` bytes to `z`. Vector
/// results land in `scratch[0]`; all mirror the interpreter's v128 memory
/// instructions exactly.
#[cfg(feature = "compile")]
fn v128_mem(store: &mut Store, mode: u64, x: u64, y: u64, z: u64, scratch: *mut u64) -> i32 {
    match mode {
        71 => {
            let b = unsafe { scratch_read_v128(scratch, 0) };
            let a = unsafe { scratch_read_v128(scratch, 2) };
            let mut out = 0u128;
            for i in 0..16usize {
                let lane = if i < 8 {
                    (x >> (8 * i)) as u8
                } else {
                    (y >> (8 * (i - 8))) as u8
                };
                let source = if lane < 16 { a } else { b };
                let byte = crate::simd::lane_bits(source, (lane & 15) as usize, 1);
                out = crate::simd::set_lane(out, i, 1, byte);
            }
            unsafe { scratch_write_v128(scratch, 0, out) };
            crate::compile::TRAP_NONE
        }
        72 => {
            let Some(op) = crate::instr::VecLoadOp::from_code(x) else {
                return gc_unsupported(store, "simd load form");
            };
            let ptr = y as *const u8;
            let size = op.bytes();
            // SAFETY: the compiled `mem_ea` bounds-checked `size` bytes at
            // this effective address, and no growth can intervene between the
            // check and this call, so the slice stays in bounds.
            let bytes = unsafe { std::slice::from_raw_parts(ptr, size) };
            match load_vec(op, bytes) {
                Some(out) => {
                    unsafe { scratch_write_v128(scratch, 0, out) };
                    crate::compile::TRAP_NONE
                }
                None => gc_unsupported(store, "simd load form"),
            }
        }
        73 => {
            let size = x as usize;
            let lane = y as u8;
            let ptr = z as *const u8;
            let vector = unsafe { scratch_read_v128(scratch, 0) };
            // SAFETY: the compiled `mem_ea` bounds-checked `size` bytes at
            // this effective address (see mode 72).
            let bytes = unsafe { std::slice::from_raw_parts(ptr, size) };
            let mut bits = 0u64;
            for (i, &byte) in bytes.iter().enumerate() {
                bits |= u64::from(byte) << (8 * i);
            }
            let out = crate::simd::set_lane(vector, lane as usize, size, bits);
            unsafe { scratch_write_v128(scratch, 0, out) };
            crate::compile::TRAP_NONE
        }
        74 => {
            let size = x as usize;
            let lane = y as u8;
            let ptr = z as *mut u8;
            let vector = unsafe { scratch_read_v128(scratch, 0) };
            let bits = crate::simd::lane_bits(vector, lane as usize, size);
            // SAFETY: the compiled `mem_ea` bounds-checked `size` bytes at
            // this effective address (see mode 72).
            let bytes = unsafe { std::slice::from_raw_parts_mut(ptr, size) };
            for (i, byte) in bytes.iter_mut().enumerate() {
                *byte = (bits >> (8 * i)) as u8;
            }
            crate::compile::TRAP_NONE
        }
        _ => gc_unsupported(store, "v128 memory op"),
    }
}

/// Decode a value of `storage` from its u64 scratch slot (a reference decodes
/// its token). V128 storage has no slot representation in the compiled subset.
#[cfg(feature = "compile")]
fn storage_from_slot(storage: StorageType, value: u64) -> Option<Value> {
    match storage {
        StorageType::I8 | StorageType::I16 | StorageType::I32 => {
            Some(Value::I32(value as u32 as i32))
        }
        StorageType::I64 => Some(Value::I64(value as i64)),
        StorageType::F32 => Some(Value::F32(value as u32)),
        StorageType::F64 => Some(Value::F64(value)),
        StorageType::Ref(reference) => value_from_call_slot(ValType::Ref(reference), value),
        StorageType::V128 => None,
    }
}

/// Rewrite the caller-owned memory descriptors at `mems` (one data-pointer +
/// byte-length `u64` pair per memory index of `instance`) from the store's
/// current cells. Called after an interpreted callee runs, since any
/// `memory.grow` it performed reallocated the memory's backing `Vec`.
#[cfg(feature = "compile")]
fn refresh_descriptors(store: &mut Store, instance: u64, mems: *mut u64) {
    let Some(cells) = store
        .instances
        .get(instance as usize)
        .map(|inst| inst.memories.clone())
    else {
        return;
    };
    for (index, cell) in cells.iter().enumerate() {
        let slot = unsafe { mems.add(2 * index) };
        match store.memories.get(*cell) {
            Some(memory) => unsafe {
                *slot = memory.bytes.as_ptr() as u64;
                *slot.add(1) = memory.bytes.len() as u64;
            },
            None => unsafe {
                *slot = 0;
                *slot.add(1) = 0;
            },
        }
    }
}

/// Seed a compiled body's `gvals` buffer from the store's global cells: one
/// u64 word per numeric global, two (lo/hi) per v128, in `cells` order — the
/// layout the compiled `do_global_get`/`set` use (cumulative word offsets).
#[cfg(feature = "compile")]
fn seed_globals(store: &Store, cells: &[usize]) -> Vec<u64> {
    let mut out = Vec::with_capacity(cells.len());
    for &cell in cells {
        match store.globals[cell] {
            Value::I32(bits) => out.push(bits as u32 as u64),
            Value::I64(bits) => out.push(bits as u64),
            Value::F32(bits) => out.push(u64::from(bits)),
            Value::F64(bits) => out.push(bits),
            Value::V128(bits) => {
                out.push(bits as u64);
                out.push((bits >> 64) as u64);
            }
            _ => out.push(0),
        }
    }
    out
}

/// Flush a compiled body's `gvals` buffer back to the store's global cells,
/// consuming the same per-type word layout [`seed_globals`] produced.
#[cfg(feature = "compile")]
fn flush_globals(store: &mut Store, cells: &[usize], words: &[u64]) {
    let mut word = 0usize;
    for &cell in cells {
        let value = match store.global_types[cell].value {
            ValType::I32 => {
                let bits = words[word];
                word += 1;
                Value::I32(bits as u32 as i32)
            }
            ValType::I64 => {
                let bits = words[word];
                word += 1;
                Value::I64(bits as i64)
            }
            ValType::F32 => {
                let bits = words[word];
                word += 1;
                Value::F32(bits as u32)
            }
            ValType::F64 => {
                let bits = words[word];
                word += 1;
                Value::F64(bits)
            }
            ValType::V128 => {
                let lo = words[word] as u128;
                let hi = words[word + 1] as u128;
                word += 2;
                Value::V128(lo | (hi << 64))
            }
            _ => {
                // A compiled body never touches a non-carried global type;
                // keep the cursor in lockstep defensively.
                word += 1;
                continue;
            }
        };
        store.globals[cell] = value;
    }
}

/// A compiled `global.get` (mode 17) / `global.set` (mode 18) over any global
/// index-space entry — imported cells included. An imported cell can alias
/// another import slot, and a call-bearing body's writes must be visible to
/// (and reads fresh from) callees, so these access the store cell the way the
/// interpreter does instead of riding the caller-owned `gvals` snapshot. A
/// `get` writes the cell's word(s) to `scratch` (two for a v128); a `set`
/// reads them back from `scratch`.
#[cfg(feature = "compile")]
fn global_op(store: &mut Store, instance: u64, mode: u64, x: u64, scratch: *mut u64) -> i32 {
    let cell = match store
        .instances
        .get(instance as usize)
        .and_then(|inst| inst.globals.get(x as usize))
        .copied()
    {
        Some(cell) => cell,
        None => {
            store.set_pending_error(ExecFail::Unsupported(
                "unresolved global in a compiled body",
            ));
            return crate::compile::TRAP_PENDING_ERROR;
        }
    };
    match mode {
        17 => {
            let words = seed_globals(store, &[cell]);
            // SAFETY: `scratch` is the caller-owned scratch region, sized for
            // any call site the body can lower (two words for a v128).
            unsafe {
                for (i, word) in words.iter().enumerate() {
                    *scratch.add(i) = *word;
                }
            }
        }
        _ => {
            let width = match store.global_types.get(cell).map(|ty| ty.value) {
                Some(ValType::V128) => 2,
                Some(_) => 1,
                None => {
                    store.set_pending_error(ExecFail::Unsupported(
                        "unresolved global in a compiled body",
                    ));
                    return crate::compile::TRAP_PENDING_ERROR;
                }
            };
            let mut words = vec![0u64; width];
            // SAFETY: `scratch` holds the value words the compiled `global.set`
            // spilled before the call.
            unsafe {
                for (i, word) in words.iter_mut().enumerate() {
                    *word = *scratch.add(i);
                }
            }
            flush_globals(store, &[cell], &words);
        }
    }
    crate::compile::TRAP_NONE
}

impl Store {
    pub fn new() -> Self {
        Store {
            instances: Vec::new(),
            host_funcs: Vec::new(),
            globals: Vec::new(),
            global_types: Vec::new(),
            memories: Vec::new(),
            memory_types: Vec::new(),
            tables: Vec::new(),
            table_types: Vec::new(),
            tags: Vec::new(),
            exceptions: Vec::new(),
            objects: Vec::new(),
            suspended: Vec::new(),
            #[cfg(feature = "compile")]
            compile_off: false,
            #[cfg(feature = "compile")]
            pending_error: None,
            #[cfg(feature = "compile")]
            native_depth: 0,
        }
    }

    /// Park a non-trap error for the running compiled call to drain.
    #[cfg(feature = "compile")]
    pub(crate) fn set_pending_error(&mut self, error: ExecFail) {
        self.pending_error = Some(error);
    }

    /// Take the parked error, if any (cleared on read).
    #[cfg(feature = "compile")]
    pub(crate) fn take_pending_error(&mut self) -> Option<ExecFail> {
        self.pending_error.take()
    }

    /// Force (or re-enable) the interpreter for this store's invocations,
    /// bypassing compiled bodies. Cut 11 equivalence tests compare the two
    /// paths on the same module.
    #[cfg(feature = "compile")]
    pub fn set_compile(&mut self, enabled: bool) {
        self.compile_off = !enabled;
    }

    /// Compiled-coverage totals across every live instance: how many
    /// module-defined functions compiled vs. the total, plus a coarse reason
    /// per function that did not (see [`crate::compile::body_compile_reason`]).
    /// An interpreter-forced store reports zeros.
    #[cfg(feature = "compile")]
    pub fn compile_coverage(&self) -> (usize, usize, Vec<&'static str>) {
        let mut compiled = 0usize;
        let mut defined = 0usize;
        let mut reasons = Vec::new();
        for instance in &self.instances {
            if instance.compiled.is_empty() {
                // Interpreter-forced store: nothing was compiled.
                continue;
            }
            let bodies = instance.module.bodies.len();
            defined += bodies;
            for (index, entry) in instance.compiled.iter().enumerate().take(bodies) {
                if entry.is_some() {
                    compiled += 1;
                } else {
                    reasons.push(crate::compile::body_compile_reason(&instance.module, index));
                }
            }
        }
        (compiled, defined, reasons)
    }

    /// Register a type-only host function (spectest `print*` family); returns
    /// its id.
    pub fn host_func(&mut self, ty: FuncType) -> usize {
        self.host_funcs.push(HostFunc { ty, token: None });
        self.host_funcs.len() - 1
    }

    /// Register an *external* host function — one whose results the caller of
    /// a resumable run supplies via [`Store::resume`]. The JS-API registers a
    /// JS closure import this way, keyed by an embedder `token`. Returns its
    /// id.
    pub fn external_host(&mut self, ty: FuncType, token: u64) -> usize {
        self.host_funcs.push(HostFunc {
            ty,
            token: Some(token),
        });
        self.host_funcs.len() - 1
    }

    /// Register a standalone tag cell (a JS-API `WebAssembly.Tag`); returns
    /// its id. The payload shape is the tag's function type (parameters only).
    pub fn tag(&mut self, ty: FuncType) -> usize {
        self.tags.push(TagInst { ty, owner: None });
        self.tags.len() - 1
    }

    /// Register a standalone global cell (spectest values); returns its id.
    pub fn global(&mut self, ty: GlobalType, value: Value) -> usize {
        self.global_types.push(ty);
        self.globals.push(value);
        self.globals.len() - 1
    }

    /// Register a standalone memory cell (spectest memory); returns its id.
    pub fn memory(&mut self, ty: MemType) -> Result<usize, ExecFail> {
        self.memory_types.push(ty);
        self.memories
            .push(Memory::new(ty.limits.min, ty.limits.max));
        Ok(self.memories.len() - 1)
    }

    /// Register a standalone table cell (spectest table); returns its id.
    pub fn table(&mut self, ty: TableType, init: RefValue) -> Result<usize, ExecFail> {
        self.table_types.push(ty);
        self.tables
            .push(TableInst::new(ty.limits.min, ty.limits.max, init));
        Ok(self.tables.len() - 1)
    }

    /// Which module-defined bodies of the instance being instantiated can
    /// reach an external host function through their direct callee graph (Cut
    /// 11 Wave 3's one-host-call-mechanism rule): a native run cannot suspend
    /// at an external host boundary, so those bodies are not compiled and fall
    /// back to the interpreter's parked-run protocol. `funcs` is the new
    /// instance's full function index space (resolved imports, then its own
    /// defined bodies).
    #[cfg(feature = "compile")]
    fn host_reachable_bodies(&self, module: &Module, funcs: &[FuncTarget]) -> Vec<bool> {
        let imported = module
            .imports
            .iter()
            .filter(|import| matches!(&import.desc, ImportDesc::Func(_)))
            .count();
        // DFS states over the module's own bodies: 0 unvisited, 1 on the
        // current path, 2 resolved (`result[defined]` holds the answer). Only
        // this module's bodies can cycle: every cross-instance edge points at
        // an earlier instance, whose answers are already final.
        let mut result = vec![false; module.bodies.len()];
        let mut state = vec![0u8; module.bodies.len()];
        for defined in 0..module.bodies.len() {
            self.host_reachable_of(module, funcs, imported, defined, &mut state, &mut result);
        }
        result
    }

    /// Whether executing body `defined` (of the instance `host_reachable_bodies`
    /// is analyzing) can reach an external host, following `Call`/`ReturnCall`
    /// through the resolved function index space.
    #[cfg(feature = "compile")]
    fn host_reachable_of(
        &self,
        module: &Module,
        funcs: &[FuncTarget],
        imported: usize,
        defined: usize,
        state: &mut [u8],
        result: &mut [bool],
    ) -> bool {
        if state[defined] == 2 {
            return result[defined];
        }
        if state[defined] == 1 {
            // A back edge to a body still being resolved contributes nothing:
            // any cycle member that reaches a host reports it when it finishes.
            return false;
        }
        state[defined] = 1;
        let mut reachable = false;
        for instr in &module.bodies[defined].body {
            let index = match instr {
                Instr::Call(index) | Instr::ReturnCall(index) => *index as usize,
                _ => continue,
            };
            if index < imported {
                match funcs.get(index) {
                    // A direct call to a token'd host import (a JS-API
                    // closure) would park the run; the compiled path cannot.
                    Some(FuncTarget::Host(id))
                        if self
                            .host_funcs
                            .get(*id)
                            .is_some_and(|host| host.token.is_some()) =>
                    {
                        reachable = true;
                    }
                    // A direct call into an earlier instance's body: its
                    // answers are already final.
                    Some(FuncTarget::Owned { instance, defined })
                        if self
                            .instances
                            .get(*instance)
                            .and_then(|inst| inst.host_reachable.get(*defined))
                            .copied()
                            .unwrap_or(false) =>
                    {
                        reachable = true;
                    }
                    Some(_) | None => {}
                }
            } else if self.host_reachable_of(
                module,
                funcs,
                imported,
                index - imported,
                state,
                result,
            ) {
                reachable = true;
            }
            if reachable {
                break;
            }
        }
        state[defined] = 2;
        result[defined] = reachable;
        reachable
    }

    /// Instantiate `module`, resolving each import through `resolve` (module
    /// name, field name) to an [`ExternVal`] or `None` for an unknown import.
    /// Returns the new instance's id.
    pub fn instantiate(
        &mut self,
        module: &Module,
        resolve: &mut dyn FnMut(&str, &str) -> Option<ExternVal>,
    ) -> Result<usize, InstantiateError> {
        let self_id = self.instances.len();
        let mut funcs = Vec::with_capacity(module.imports.len() + module.functions.len());
        let mut tables = Vec::with_capacity(module.imports.len());
        let mut globals = Vec::with_capacity(module.imports.len());
        let mut memories = Vec::with_capacity(module.imports.len());
        let mut tags = Vec::with_capacity(module.imports.len());

        for import in &module.imports {
            match &import.desc {
                ImportDesc::Func(type_index) => {
                    let value = resolve(&import.module, &import.name)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    match value {
                        ExternVal::HostFunc(id) => {
                            let target = FuncTarget::Host(id);
                            if !self.func_type_matches(module, *type_index, target) {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            funcs.push(FuncTarget::Host(id));
                        }
                        ExternVal::Func { instance, index } => {
                            let target = *self
                                .instances
                                .get(instance)
                                .and_then(|i| i.funcs.get(index))
                                .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                            if !self.func_type_matches(module, *type_index, target) {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            funcs.push(target);
                        }
                        ExternVal::Unsupported(reason) => {
                            return Err(InstantiateError::Unsupported(reason));
                        }
                        _ => {
                            return Err(InstantiateError::Unlinkable("incompatible import type"));
                        }
                    }
                }
                ImportDesc::Global(gty) => {
                    let value = resolve(&import.module, &import.name)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    match value {
                        ExternVal::Global(cell) => {
                            let actual = *self
                                .global_types
                                .get(cell)
                                .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                            // Mutability must match exactly. An immutable
                            // import's value type may subsume (a non-null
                            // `(ref func)` satisfies `funcref`), but a mutable
                            // global is an invariant container: the importer
                            // could write into it, so the value type must be
                            // identical.
                            let value_ok = if gty.mutable {
                                gty.value == actual.value
                            } else {
                                matches_val(gty.value, actual.value)
                            };
                            if actual.mutable != gty.mutable || !value_ok {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            globals.push(cell);
                        }
                        ExternVal::Unsupported(reason) => {
                            return Err(InstantiateError::Unsupported(reason));
                        }
                        _ => {
                            return Err(InstantiateError::Unlinkable("incompatible import type"));
                        }
                    }
                }
                ImportDesc::Memory(mt) => {
                    let value = resolve(&import.module, &import.name)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    match value {
                        ExternVal::Memory(cell) => {
                            let actual = self.memory_type_of(cell);
                            if !memory_matches(actual, *mt) {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            memories.push(cell);
                        }
                        ExternVal::Unsupported(reason) => {
                            return Err(InstantiateError::Unsupported(reason));
                        }
                        _ => {
                            return Err(InstantiateError::Unlinkable("incompatible import type"));
                        }
                    }
                }
                ImportDesc::Table(tt) => {
                    let value = resolve(&import.module, &import.name)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    match value {
                        ExternVal::Table(cell) => {
                            let actual = self.table_type_of(cell);
                            if !table_matches(actual, *tt) {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            tables.push(cell);
                        }
                        ExternVal::Unsupported(reason) => {
                            return Err(InstantiateError::Unsupported(reason));
                        }
                        _ => {
                            return Err(InstantiateError::Unlinkable("incompatible import type"));
                        }
                    }
                }
                ImportDesc::Tag(type_index) => {
                    let expected = module
                        .func_at_cloned(*type_index)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    let value = resolve(&import.module, &import.name)
                        .ok_or(InstantiateError::Unlinkable("unknown import"))?;
                    match value {
                        ExternVal::Tag(cell) => {
                            // Tags match by payload shape: the imported type
                            // must be equivalent to the tag's declared type
                            // (cross-module rec-group aware when the tag was
                            // declared by a module).
                            let ok = match self.tags.get(cell).and_then(|t| t.owner) {
                                Some((owner_instance, owner_type)) => {
                                    self.instances.get(owner_instance).is_some_and(|owner| {
                                        crate::module::type_indices_equivalent(
                                            &module.types,
                                            &module.rec_groups,
                                            *type_index,
                                            &owner.module.types,
                                            &owner.module.rec_groups,
                                            owner_type,
                                        )
                                    })
                                }
                                None => self.tags.get(cell).map(|t| t.ty.clone()) == Some(expected),
                            };
                            if !ok {
                                return Err(InstantiateError::Unlinkable(
                                    "incompatible import type",
                                ));
                            }
                            tags.push(cell);
                        }
                        ExternVal::Unsupported(reason) => {
                            return Err(InstantiateError::Unsupported(reason));
                        }
                        _ => {
                            return Err(InstantiateError::Unlinkable("incompatible import type"));
                        }
                    }
                }
            }
        }

        for (defined, _) in module.functions.iter().enumerate() {
            funcs.push(FuncTarget::Owned {
                instance: self_id,
                defined,
            });
        }

        for type_index in &module.tags {
            let ty = module
                .func_at_cloned(*type_index)
                .ok_or(InstantiateError::Unlinkable("unknown tag type"))?;
            self.tags.push(TagInst {
                ty,
                owner: Some((self_id, *type_index)),
            });
            tags.push(self.tags.len() - 1);
        }

        for table in &module.tables {
            let init = if let Some(init_expr) = &table.init {
                let values = self.global_values(&globals);
                eval_ref_const(self, module, init_expr, &values, self_id)?
            } else {
                RefValue::Null
            };
            self.table_types.push(table.ty);
            self.tables.push(TableInst::new(
                table.ty.limits.min,
                table.ty.limits.max,
                init,
            ));
            tables.push(self.tables.len() - 1);
        }

        for memory in &module.memories {
            self.memory_types.push(*memory);
            self.memories
                .push(Memory::new(memory.limits.min, memory.limits.max));
            memories.push(self.memories.len() - 1);
        }

        for global in &module.globals {
            let values = self.global_values(&globals);
            let value = eval_const(self, module, &global.init, &values, self_id)
                .map_err(InstantiateError::from)?;
            self.global_types.push(global.ty);
            self.globals.push(value);
            globals.push(self.globals.len() - 1);
        }

        // Cut 11 Wave 3: bodies whose callee graph can reach an external host
        // function are not compiled (they run interpreted, whose parked-run
        // protocol is the one host-call mechanism).
        #[cfg(feature = "compile")]
        let host_reachable = if self.compile_off {
            Vec::new()
        } else {
            self.host_reachable_bodies(module, &funcs)
        };

        self.instances.push(Instance {
            module: module.clone(),
            funcs,
            tables,
            globals,
            memories,
            tags,
            element_segments: Vec::new(),
            data_segments: Vec::new(),
            depth_limit: DEFAULT_DEPTH_LIMIT,
            #[cfg(feature = "compile")]
            compiled: if self.compile_off {
                // Interpreter-forced store: don't pay the compile cost at
                // all (the equivalence harness runs whole corpora twice).
                Vec::new()
            } else {
                let mut compiled = crate::compile::compile_module(module);
                for (entry, reachable) in compiled.iter_mut().zip(&host_reachable) {
                    if *reachable {
                        *entry = None;
                    }
                }
                compiled
            },
            #[cfg(feature = "compile")]
            host_reachable,
        });

        // Instantiate element segments in order (all elements before data),
        // then data segments. Writes to imported tables/memories land in the
        // shared cells and persist even if a later segment traps.
        for segment in &module.elements {
            let values = self.global_values(&self.instances[self_id].globals);
            let items: Vec<RefValue> = segment
                .init
                .iter()
                .map(|item| eval_ref_const(self, module, item, &values, self_id))
                .collect::<Result<_, _>>()?;
            match &segment.mode {
                ElementMode::Active { table, offset } => {
                    let cell = self.instances[self_id]
                        .tables
                        .get(*table as usize)
                        .copied()
                        .ok_or(InstantiateError::Trap(Trap::OutOfBoundsTableAccess))?;
                    let offset_value = eval_const(self, module, offset, &values, self_id)
                        .map_err(InstantiateError::from)?;
                    // The active offset's width follows the table's address
                    // type: i32 for a 32-bit table, i64 for a table64.
                    let start = match offset_value {
                        Value::I32(offset) => offset as u32 as usize,
                        Value::I64(offset) => offset as u64 as usize,
                        _ => {
                            return Err(InstantiateError::Unsupported("non-address table offset"));
                        }
                    };
                    let end = start
                        .checked_add(items.len())
                        .ok_or(InstantiateError::Trap(Trap::OutOfBoundsTableAccess))?;
                    let table = &mut self.tables[cell];
                    if end > table.elements.len() {
                        return Err(InstantiateError::Trap(Trap::OutOfBoundsTableAccess));
                    }
                    table.elements[start..end].copy_from_slice(&items);
                    self.instances[self_id].element_segments.push(None);
                }
                ElementMode::Passive => {
                    self.instances[self_id].element_segments.push(Some(items));
                }
                ElementMode::Declarative => {
                    self.instances[self_id].element_segments.push(None);
                }
            }
        }

        for segment in &module.data {
            if let DataMode::Active { memory, offset } = &segment.mode {
                let memory_index = *memory as usize;
                let cell = self.instances[self_id]
                    .memories
                    .get(memory_index)
                    .copied()
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))
                    .map_err(InstantiateError::from)?;
                let values = self.global_values(&self.instances[self_id].globals);
                let offset_value = eval_const(self, module, offset, &values, self_id)
                    .map_err(InstantiateError::from)?;
                // The active offset's width follows the memory's address type:
                // i32 for a memory32, i64 for a memory64.
                let start = match offset_value {
                    Value::I32(offset) => offset as u32 as usize,
                    Value::I64(offset) => offset as u64 as usize,
                    _ => {
                        return Err(InstantiateError::Unsupported("non-address data offset"));
                    }
                };
                let end = start
                    .checked_add(segment.bytes.len())
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))
                    .map_err(InstantiateError::from)?;
                let memory = &mut self.memories[cell];
                if end > memory.bytes.len() {
                    return Err(InstantiateError::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                memory.bytes[start..end].copy_from_slice(&segment.bytes);
                self.instances[self_id].data_segments.push(None);
            } else {
                self.instances[self_id]
                    .data_segments
                    .push(Some(segment.bytes.clone()));
            }
        }

        // Run the start function, if any.
        if let Some(start) = module.start {
            let target = *self.instances[self_id]
                .funcs
                .get(start as usize)
                .ok_or(ExecFail::Trap(Trap::UnknownFunction))
                .map_err(InstantiateError::from)?;
            self.run_target(target, &[])
                .map_err(InstantiateError::from)?;
        }
        Ok(self_id)
    }

    /// The export `name` of `instance` as an importable value.
    pub fn export(&self, instance: usize, name: &str) -> Option<ExternVal> {
        let inst = self.instances.get(instance)?;
        let export = inst.module.exports.iter().find(|e| e.name == name)?;
        Some(match export.kind {
            ExportKind::Func => ExternVal::Func {
                instance,
                index: export.index as usize,
            },
            ExportKind::Global => ExternVal::Global(inst.globals[export.index as usize]),
            ExportKind::Memory => ExternVal::Memory(inst.memories[export.index as usize]),
            ExportKind::Table => ExternVal::Table(inst.tables[export.index as usize]),
            ExportKind::Tag => ExternVal::Tag(inst.tags[export.index as usize]),
        })
    }

    /// All exports of `instance`, for `register`-style name resolution.
    pub fn exports(&self, instance: usize) -> Vec<(String, ExternVal)> {
        self.instances
            .get(instance)
            .map(|inst| {
                inst.module
                    .exports
                    .iter()
                    .map(|export| {
                        let value = match export.kind {
                            ExportKind::Func => ExternVal::Func {
                                instance,
                                index: export.index as usize,
                            },
                            ExportKind::Global => {
                                ExternVal::Global(inst.globals[export.index as usize])
                            }
                            ExportKind::Memory => {
                                ExternVal::Memory(inst.memories[export.index as usize])
                            }
                            ExportKind::Table => {
                                ExternVal::Table(inst.tables[export.index as usize])
                            }
                            ExportKind::Tag => ExternVal::Tag(inst.tags[export.index as usize]),
                        };
                        (export.name.clone(), value)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The full-space function index of exported function `name`, if any.
    pub fn exported_func(&self, instance: usize, name: &str) -> Option<usize> {
        self.instances
            .get(instance)?
            .module
            .exports
            .iter()
            .find_map(|export| {
                (export.name == name && export.kind == ExportKind::Func)
                    .then_some(export.index as usize)
            })
    }

    /// The canonical identity of the function at full-space `index` of
    /// `instance`: imports alias the target they resolve to, so a function
    /// re-exported through later instances keeps its defining instance's key.
    pub fn func_key(&self, instance: usize, index: usize) -> Option<FuncKey> {
        let inst = self.instances.get(instance)?;
        let target = inst.funcs.get(index)?;
        Some(match target {
            FuncTarget::Host(id) => FuncKey::Host(*id),
            FuncTarget::Owned { instance, defined } => FuncKey::Owned {
                instance: *instance,
                defined: *defined,
            },
        })
    }

    /// The current value of exported global `name`, if any.
    pub fn export_global_value(&self, instance: usize, name: &str) -> Option<Value> {
        let inst = self.instances.get(instance)?;
        let export = inst
            .module
            .exports
            .iter()
            .find(|e| e.name == name && e.kind == ExportKind::Global)?;
        let cell = inst.globals[export.index as usize];
        self.globals.get(cell).copied()
    }

    /// The declared function type at `index` (full function index space) of
    /// `instance`, if any. Used by the runner to shape reference arguments
    /// (a `ref.host` payload argument is an internal `any` value, while a
    /// `ref.extern` payload is wrapped as an external).
    pub fn func_type(&self, instance: usize, index: usize) -> Option<FuncType> {
        let inst = self.instances.get(instance)?;
        let func_imports = inst
            .module
            .imports
            .iter()
            .filter(|import| matches!(import.desc, ImportDesc::Func(_)))
            .count();
        let ti = if index < func_imports {
            inst.module
                .imports
                .iter()
                .filter_map(|import| match import.desc {
                    ImportDesc::Func(ty) => Some(ty),
                    _ => None,
                })
                .nth(index)
        } else {
            inst.module.functions.get(index - func_imports).copied()
        }?;
        inst.module.func_at_cloned(ti)
    }

    /// The parameter types of function `index` (full function index space) of
    /// `instance`.
    pub fn func_params(&self, instance: usize, index: usize) -> Option<Vec<ValType>> {
        self.func_type(instance, index).map(|ty| ty.params)
    }

    /// Invoke function `index` (full index space) of `instance` with `args`.
    /// Non-resumable: an external host function reached mid-run (possible only
    /// when a module imported a JS-API closure) fails as unsupported here —
    /// the JS-API uses [`Store::start`]/[`Store::resume`] instead.
    pub fn invoke(
        &mut self,
        instance: usize,
        index: usize,
        args: &[Value],
    ) -> Result<Vec<Value>, ExecFail> {
        let target = *self
            .instances
            .get(instance)
            .and_then(|i| i.funcs.get(index))
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        self.run_target(target, args)
    }

    /// Start a resumable invocation of function `index` (full index space) of
    /// `instance` with `args`, running until the next external-host-call
    /// boundary or completion. A [`RunProgress::Host`] result parks the run
    /// inside the store; the caller runs the embedder host function with the
    /// store unborrowed and passes its results to [`Store::resume`]. Reentrant
    /// invocations simply nest: each suspension pushes, each resume pops.
    pub fn start(
        &mut self,
        instance: usize,
        index: usize,
        args: &[Value],
    ) -> Result<RunProgress, ExecFail> {
        let target = *self
            .instances
            .get(instance)
            .and_then(|i| i.funcs.get(index))
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        self.start_target(target, args)
    }

    /// Resume the most recently suspended run with an external host function's
    /// results (or an error, which abandons that run and propagates).
    pub fn resume(&mut self, reply: Result<Vec<Value>, ExecFail>) -> Result<RunProgress, ExecFail> {
        let suspended = self
            .suspended
            .pop()
            .ok_or(ExecFail::Unsupported("resume without a suspended run"))?;
        let results = reply?;
        let mut engine = Engine {
            store: self,
            stack: suspended.stack,
            frames: suspended.frames,
        };
        engine.stack.extend(results);
        engine.run()
    }

    /// Discard the most recently suspended run (its external host function
    /// threw and the caller abandons the invocation it was part of). Returns
    /// whether there was a run to discard.
    pub fn abandon(&mut self) -> bool {
        self.suspended.pop().is_some()
    }

    /// Resume the most recently suspended run with an in-flight exception
    /// instead of host results: the exception unwinds inside wasm (a matching
    /// `try_table` catches it, or it escapes to this caller as
    /// [`ExecFail::Exception`]). Used when an imported JS function throws a
    /// `WebAssembly.Exception` whose tag the wasm frames can catch.
    pub fn resume_exception(&mut self, exn: usize) -> Result<RunProgress, ExecFail> {
        let suspended = self
            .suspended
            .pop()
            .ok_or(ExecFail::Unsupported("resume without a suspended run"))?;
        let mut engine = Engine {
            store: self,
            stack: suspended.stack,
            frames: suspended.frames,
        };
        // No wasm frames are left to catch (a top-level external-host call or
        // an outermost tail call): the exception escapes immediately.
        if engine.frames.is_empty() {
            return Err(ExecFail::Exception(exn));
        }
        match engine.unwind(exn)? {
            Some(results) => Ok(RunProgress::Finished(results)),
            None => engine.run(),
        }
    }

    fn run_target(&mut self, target: FuncTarget, args: &[Value]) -> Result<Vec<Value>, ExecFail> {
        match target {
            FuncTarget::Owned { instance, defined } => self.run_owned(instance, defined, args),
            FuncTarget::Host(id) => {
                if self.host_funcs[id].token.is_some() {
                    return Err(ExecFail::Unsupported(
                        "external host function needs a resumable run",
                    ));
                }
                let ty = self.host_funcs[id].ty.clone();
                if ty.params.len() != args.len() {
                    return Err(ExecFail::Unsupported("host function arity"));
                }
                if !ty.results.is_empty() {
                    return Err(ExecFail::Unsupported("host function results"));
                }
                Ok(Vec::new())
            }
        }
    }

    /// Start a resumable run against `target`. An external host target parks
    /// an empty run so [`Store::resume`] delivers its results directly as the
    /// invocation's; a type-only host target finishes immediately.
    fn start_target(
        &mut self,
        target: FuncTarget,
        args: &[Value],
    ) -> Result<RunProgress, ExecFail> {
        match target {
            FuncTarget::Owned { instance, defined } => self.start_owned(instance, defined, args),
            FuncTarget::Host(id) => {
                let ty = self.host_funcs[id].ty.clone();
                if ty.params.len() != args.len() {
                    return Err(ExecFail::Unsupported("host function arity"));
                }
                match self.host_funcs[id].token {
                    Some(token) => {
                        self.suspended.push(Suspended {
                            stack: Vec::new(),
                            frames: Vec::new(),
                        });
                        Ok(RunProgress::Host(HostRequest {
                            token,
                            args: args.to_vec(),
                        }))
                    }
                    None => {
                        if !ty.results.is_empty() {
                            return Err(ExecFail::Unsupported("host function results"));
                        }
                        Ok(RunProgress::Finished(Vec::new()))
                    }
                }
            }
        }
    }

    /// Run a module-defined function by its index into `Module::bodies`.
    fn run_owned(
        &mut self,
        instance: usize,
        defined: usize,
        args: &[Value],
    ) -> Result<Vec<Value>, ExecFail> {
        match self.start_owned(instance, defined, args)? {
            RunProgress::Finished(results) => Ok(results),
            // A non-resumable caller reached an external host function (only
            // possible through an instance start function): discard the parked
            // run and report the limitation.
            RunProgress::Host(_) => {
                self.suspended.pop();
                Err(ExecFail::Unsupported(
                    "external host function in a non-resumable run",
                ))
            }
        }
    }

    /// Construct and run a root frame for `defined` of `instance`, parking at
    /// the next external-host boundary instead of failing.
    fn start_owned(
        &mut self,
        instance: usize,
        defined: usize,
        args: &[Value],
    ) -> Result<RunProgress, ExecFail> {
        let (signature, declared) = {
            let module = &self.instances[instance].module;
            let body = module
                .bodies
                .get(defined)
                .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
            let type_index = module
                .functions
                .get(defined)
                .copied()
                .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
            let signature = module.func_at_cloned(type_index);
            let signature = signature.ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
            (signature, body.locals.clone())
        };
        // Cut 11: a compiled leaf entry runs synchronously (no host boundary
        // in the current subset), so it is a `Finished` run like an
        // interpreter run that never suspends. `set_compile(false)` forces
        // the interpreter for equivalence testing.
        #[cfg(feature = "compile")]
        let compiled = if self.compile_off {
            None
        } else {
            self.instances[instance]
                .compiled
                .get(defined)
                .and_then(|entry| entry.as_ref())
                .map(|func| func as *const crate::compile::CompiledFunc)
        };
        #[cfg(feature = "compile")]
        if let Some(func_ptr) = compiled {
            // SAFETY: the instance (and its compiled entries) is stable for
            // this whole method — nothing below pushes to `instances`. The
            // reference is only used to reach the native entry and the
            // function's global metadata while we mutate global/memory cells.
            let func = unsafe { &*func_ptr };
            // Per-memory descriptors (data pointer, byte length) for the
            // module's whole memory index space, caller-owned for the leaf
            // call. A compiled `memory.grow` reallocates a cell's backing
            // `Vec` through the runtime helper, which refreshes the
            // descriptor entry in place; `desc` pointers stay valid because
            // this array is never resized mid-call.
            let memory_cells = self.instances[instance].memories.clone();
            let mut descriptors: Vec<u64> = Vec::with_capacity(2 * memory_cells.len());
            for cell in memory_cells {
                match self.memories.get(cell) {
                    Some(memory) => {
                        descriptors.push(memory.bytes.as_ptr() as u64);
                        descriptors.push(memory.bytes.len() as u64);
                    }
                    None => {
                        descriptors.push(0);
                        descriptors.push(0);
                    }
                }
            }
            let mems = (descriptors.as_ptr(), descriptors.len() as u64);
            // The used globals' current bits ride a caller-owned buffer: the
            // compiled body reads and mutates slots in place, so its writes
            // are visible to the store even when it traps.
            let used = func.used_globals().to_vec();
            let cells = used
                .iter()
                .map(|index| self.instances[instance].globals[*index as usize])
                .collect::<Vec<_>>();
            let mut globals = seed_globals(self, &cells);
            // Runtime pointers the compiled body can call back into: the
            // store address and instance index for the `memory.grow` and call
            // helpers, the helpers' code addresses, and a caller-owned
            // scratch region for call argument/result slots (entry params
            // 7-11).
            let scratch = vec![0u64; crate::compile::SCRATCH_SLOTS];
            let runtime = crate::compile::CompiledRuntime {
                store: self as *mut Store as u64,
                instance: instance as u64,
                grow: memory_grow_helper as *const () as usize as u64,
                call: wasm_call_helper as *const () as usize as u64,
                scratch: scratch.as_ptr() as u64,
            };
            let result =
                crate::compile::run_compiled(func, &signature, mems, &mut globals, args, runtime);
            flush_globals(self, &cells, &globals);
            return Ok(RunProgress::Finished(result?));
        }
        let mut locals = args.to_vec();
        for ty in &declared {
            locals.push(default_value(*ty)?);
        }
        let results = signature.results.len();
        let mut engine = Engine {
            store: self,
            stack: Vec::new(),
            frames: vec![Frame {
                instance,
                pc: 0,
                locals,
                base: 0,
                results,
                body_index: defined,
                labels: vec![Label {
                    arity: results,
                    height: 0,
                    is_loop: false,
                    open: 0,
                }],
            }],
        };
        engine.run()
    }

    /// Whether a resolved function target satisfies a declared module type
    /// index: an owned function's type must be rec-group equivalent across the
    /// two module type spaces; a host function's signature must equal the
    /// declared one (host signatures carry no module-defined types).
    fn func_type_matches(&self, module: &Module, type_index: u32, target: FuncTarget) -> bool {
        match target {
            FuncTarget::Host(id) => {
                let declared = module.func_at_cloned(type_index).unwrap_or(FuncType {
                    params: Vec::new(),
                    results: Vec::new(),
                });
                self.host_funcs[id].ty == declared
            }
            FuncTarget::Owned { instance, defined } => {
                let Some(owner) = self.instances.get(instance) else {
                    return false;
                };
                let Some(&owner_type) = owner.module.functions.get(defined) else {
                    return false;
                };
                crate::module::type_indices_equivalent(
                    &module.types,
                    &module.rec_groups,
                    type_index,
                    &owner.module.types,
                    &owner.module.rec_groups,
                    owner_type,
                )
            }
        }
    }

    fn global_values(&self, cells: &[usize]) -> Vec<Value> {
        cells.iter().map(|&cell| self.globals[cell]).collect()
    }

    /// A memory cell's type for import matching: the declared maximum but the
    /// *current* size as the minimum (a grown memory can satisfy a larger
    /// import minimum).
    fn memory_type_of(&self, cell: usize) -> MemType {
        let declared = self.memory_types[cell];
        MemType {
            limits: Limits {
                min: self.memories[cell].pages(),
                max: declared.limits.max,
                shared: declared.limits.shared,
            },
            memory64: declared.memory64,
        }
    }

    /// A table cell's type for import matching (current size as the minimum).
    fn table_type_of(&self, cell: usize) -> TableType {
        let declared = self.table_types[cell];
        TableType {
            element: declared.element,
            limits: Limits {
                min: self.tables[cell].elements.len() as u64,
                max: declared.limits.max,
                shared: declared.limits.shared,
            },
            table64: declared.table64,
        }
    }

    // ---- cell accessors for the JS-API wrapper objects (Cut 10 wave 3b) ----
    //
    // The runtime's `WebAssembly.Memory`/`Table`/`Global` wrappers hold these
    // store cell ids and read/write the shared cells through this surface so
    // imports alias the exporter's state.

    /// A global cell's declared type.
    pub fn global_type(&self, cell: usize) -> Option<GlobalType> {
        self.global_types.get(cell).copied()
    }

    /// A memory cell's declared type.
    pub fn memory_type(&self, cell: usize) -> Option<MemType> {
        self.memory_types.get(cell).copied()
    }

    /// A table cell's declared type.
    pub fn table_type(&self, cell: usize) -> Option<TableType> {
        self.table_types.get(cell).copied()
    }

    /// A global cell's current value.
    pub fn global_value(&self, cell: usize) -> Option<Value> {
        self.globals.get(cell).copied()
    }

    /// Overwrite a global cell. The caller is responsible for checking
    /// mutability and value-type compatibility first.
    pub fn set_global(&mut self, cell: usize, value: Value) -> bool {
        match self.globals.get_mut(cell) {
            Some(slot) => {
                *slot = value;
                true
            }
            None => false,
        }
    }

    /// A tag cell's declared payload type.
    pub fn tag_type(&self, cell: usize) -> Option<FuncType> {
        self.tags.get(cell).map(|tag| tag.ty.clone())
    }

    /// Allocate a thrown exception instance carrying `tag`'s payload `args`;
    /// returns the exception id an escaping run reports. Used both by the
    /// interpreter's `throw` and by the JS-API when a JS-thrown
    /// `WebAssembly.Exception` enters wasm.
    pub fn new_exception(&mut self, tag: usize, args: Vec<Value>) -> usize {
        self.exceptions.push(ExceptionInst { tag, args });
        self.exceptions.len() - 1
    }

    /// The tag cell an exception instance was thrown with.
    pub fn exception_tag(&self, id: usize) -> Option<usize> {
        self.exceptions.get(id).map(|exception| exception.tag)
    }

    /// The payload of an exception instance.
    pub fn exception_args(&self, id: usize) -> Option<Vec<Value>> {
        self.exceptions
            .get(id)
            .map(|exception| exception.args.clone())
    }

    /// A memory cell's current size in pages.
    pub fn memory_size(&self, cell: usize) -> Option<u64> {
        self.memories.get(cell).map(Memory::pages)
    }

    /// Grow a memory cell by `delta` pages, enforcing the cell's declared
    /// maximum (and the memory32 2^16-page cap); the old size in pages on
    /// success.
    pub fn grow_memory(&mut self, cell: usize, delta: u64) -> Option<u64> {
        let memory64 = self.memory_types.get(cell)?.memory64;
        self.memories.get_mut(cell)?.grow(delta, memory64)
    }

    /// A memory cell's current bytes (the whole linear memory).
    pub fn memory_bytes(&self, cell: usize) -> Option<&[u8]> {
        self.memories
            .get(cell)
            .map(|memory| memory.bytes.as_slice())
    }

    /// Overwrite a whole memory cell. The caller matches lengths; returns
    /// false when the cell is unknown or the byte slice does not fill it.
    pub fn write_memory(&mut self, cell: usize, bytes: &[u8]) -> bool {
        match self.memories.get_mut(cell) {
            Some(memory) if memory.bytes.len() == bytes.len() => {
                memory.bytes.copy_from_slice(bytes);
                true
            }
            _ => false,
        }
    }

    /// A table cell's current length.
    pub fn table_size(&self, cell: usize) -> Option<u64> {
        self.tables
            .get(cell)
            .map(|table| table.elements.len() as u64)
    }

    /// Read one table slot (an out-of-bounds index is a table-access trap).
    pub fn table_get(&self, cell: usize, index: u64) -> Option<Result<RefValue, Trap>> {
        let table = self.tables.get(cell)?;
        Some(match table.elements.get(index as usize) {
            Some(value) => Ok(*value),
            None => Err(Trap::OutOfBoundsTableAccess),
        })
    }

    /// Write one table slot (an out-of-bounds index is a table-access trap).
    pub fn table_set(
        &mut self,
        cell: usize,
        index: u64,
        value: RefValue,
    ) -> Option<Result<(), Trap>> {
        let table = self.tables.get_mut(cell)?;
        Some(match table.elements.get_mut(index as usize) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(Trap::OutOfBoundsTableAccess),
        })
    }

    /// Grow a table cell by `delta` slots, filling with `init`; the old
    /// length on success (a 32-bit table caps at 2^32-1 slots).
    pub fn grow_table(&mut self, cell: usize, delta: u64, init: RefValue) -> Option<u64> {
        let table64 = self.table_types.get(cell)?.table64;
        self.tables.get_mut(cell)?.grow(delta, init, table64)
    }
}

/// One invocation parked at an external-host-call boundary: the operand and
/// frame stacks exactly as the engine left them ([`Store::resume`] pops the
/// entry, pushes the host results, and continues).
struct Suspended {
    stack: Vec<Value>,
    frames: Vec<Frame>,
}

/// A function frame on the machine's call stack.
struct Frame {
    /// The instance whose module (code) and cells this frame executes in.
    instance: usize,
    pc: usize,
    locals: Vec<Value>,
    /// Operand-stack height when this frame's body started (arguments
    /// popped; results are produced above this line).
    base: usize,
    /// Number of result values this function produces.
    results: usize,
    /// Defined-function index into `Module::bodies` of the frame's instance.
    body_index: usize,
    /// Control labels of the current function (the function label is the
    /// bottom-most entry).
    labels: Vec<Label>,
}

#[derive(Clone, Copy)]
struct Label {
    /// Values a branch to this label carries (results for block/if/func,
    /// parameters for loop).
    arity: usize,
    /// Operand-stack height below this construct's inputs.
    height: usize,
    /// Whether branching to this label restarts the body (loop) or exits it.
    is_loop: bool,
    /// Instruction index of the opening `block`/`loop`/`if`.
    open: usize,
}

struct Engine<'a> {
    store: &'a mut Store,
    stack: Vec<Value>,
    frames: Vec<Frame>,
}

/// What the next engine iteration must do.
enum Ctl {
    /// Advance the top frame's pc by one.
    Next,
    /// The instruction already set the pc / pushed or popped a frame.
    Settled,
    /// The whole invocation finished.
    Finished(Vec<Value>),
    /// An external host function was called: the run must park and let the
    /// resumable driver supply its results (`token`, `args`).
    Host { token: u64, args: Vec<Value> },
}

/// Where an in-flight exception's unwind stopped.
enum CatchResult {
    /// No clause in the scanned frame matched; pop the frame and keep going.
    Miss,
    /// A clause matched and branched; execution continues (pc already set).
    Caught,
    /// A clause targeted the outermost frame's function label.
    Finished(Vec<Value>),
}

/// Per-body maps: the matching `End` for each opening structured
/// instruction, and the `Else` (if any) for each `if`.
struct BodyMap {
    end: Vec<usize>,
    else_: Vec<Option<usize>>,
}

fn precompute(body: &[Instr]) -> BodyMap {
    let mut end = vec![usize::MAX; body.len()];
    let mut else_ = vec![None; body.len()];
    let mut opens: Vec<usize> = Vec::new();
    for (pc, instr) in body.iter().enumerate() {
        match instr {
            Instr::Block(_) | Instr::Loop(_) | Instr::If(_) | Instr::TryTable { .. } => {
                opens.push(pc)
            }
            Instr::Else => {
                if let Some(&open) = opens.last() {
                    else_[open] = Some(pc);
                }
            }
            Instr::End => {
                if let Some(open) = opens.pop() {
                    end[open] = pc;
                }
            }
            _ => {}
        }
    }
    BodyMap { end, else_ }
}

impl<'a> Engine<'a> {
    fn body(&self, frame_index: usize) -> &[Instr] {
        &self.store.instances[self.frames[frame_index].instance]
            .module
            .bodies[self.frames[frame_index].body_index]
            .body
    }

    fn pop(&mut self) -> Result<Value, ExecFail> {
        self.stack.pop().ok_or(ExecFail::Trap(Trap::Unreachable))
    }

    fn pop_i32(&mut self) -> Result<i32, ExecFail> {
        match self.pop()? {
            Value::I32(v) => Ok(v),
            _ => Err(ExecFail::Trap(Trap::Unreachable)),
        }
    }

    fn pop_i64(&mut self) -> Result<i64, ExecFail> {
        match self.pop()? {
            Value::I64(v) => Ok(v),
            _ => Err(ExecFail::Trap(Trap::Unreachable)),
        }
    }

    /// The store cell a memory instruction's `memidx` resolves to.
    fn mem_cell(&self, instance: usize, memory: u32) -> Result<usize, ExecFail> {
        self.store.instances[instance]
            .memories
            .get(memory as usize)
            .copied()
            .ok_or(ExecFail::Unsupported("no memory"))
    }

    /// Whether the memory at `memidx` is a memory64 (its address operand and
    /// `memory.size/grow` values are i64).
    fn memory_is64(&self, instance: usize, memory: u32) -> Result<bool, ExecFail> {
        let cell = self.mem_cell(instance, memory)?;
        Ok(self.store.memory_types[cell].memory64)
    }

    /// Pop a memory instruction's address operand, sized by the referenced
    /// memory's index type (i32 for a memory32, i64 for a memory64), widened
    /// to the unsigned u64 used for effective-address arithmetic.
    fn pop_mem_addr(&mut self, instance: usize, memory: u32) -> Result<u64, ExecFail> {
        if self.memory_is64(instance, memory)? {
            Ok(self.pop_i64()? as u64)
        } else {
            Ok(self.pop_i32()? as u32 as u64)
        }
    }

    /// The table cell the top frame addresses at `table_index`.
    fn table_cell(&self, frame_index: usize, table_index: usize) -> Result<usize, ExecFail> {
        let instance = self.frames[frame_index].instance;
        self.store.instances[instance]
            .tables
            .get(table_index)
            .copied()
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsTableAccess))
    }

    /// Whether the table at `table_index` is a table64 (its address operands
    /// and `table.size/grow` values are i64).
    fn table_is64(&self, frame_index: usize, table_index: usize) -> Result<bool, ExecFail> {
        let cell = self.table_cell(frame_index, table_index)?;
        Ok(self.store.table_types[cell].table64)
    }

    /// Pop a table instruction's address operand, sized by the referenced
    /// table's index type (i32 for a 32-bit table, i64 for a table64),
    /// widened to the unsigned u64 used for bounds checks.
    fn pop_table_addr(&mut self, frame_index: usize, table_index: usize) -> Result<u64, ExecFail> {
        if self.table_is64(frame_index, table_index)? {
            Ok(self.pop_i64()? as u64)
        } else {
            Ok(self.pop_i32()? as u32 as u64)
        }
    }

    /// Run until the outermost frame returns (or an external host function is
    /// reached, parking the run so the resumable driver can supply its
    /// results). A `throw`/`throw_ref` inside any frame becomes an in-flight
    /// exception that unwinds to the nearest matching `try_table` catch
    /// clause (across call frames); only an exception no handler matches
    /// reaches the caller of `run`.
    fn run(&mut self) -> Result<RunProgress, ExecFail> {
        loop {
            // A parked run with no frames left (a top-level external-host
            // invocation, or an outermost tail call to one) is done: the
            // operand stack holds the results.
            if self.frames.is_empty() {
                return Ok(RunProgress::Finished(self.stack.split_off(0)));
            }
            let ctl = match self.step() {
                Ok(ctl) => ctl,
                Err(ExecFail::Exception(exn)) => match self.unwind(exn)? {
                    Some(results) => return Ok(RunProgress::Finished(results)),
                    None => continue,
                },
                Err(error) => return Err(error),
            };
            match ctl {
                Ctl::Next => {
                    let top = self.frames.len() - 1;
                    self.frames[top].pc += 1;
                }
                Ctl::Settled => {}
                Ctl::Finished(results) => return Ok(RunProgress::Finished(results)),
                Ctl::Host { token, args } => {
                    self.store.suspended.push(Suspended {
                        stack: std::mem::take(&mut self.stack),
                        frames: std::mem::take(&mut self.frames),
                    });
                    return Ok(RunProgress::Host(HostRequest { token, args }));
                }
            }
        }
    }

    /// Deliver an in-flight exception (a cell in `store.exceptions`) to the
    /// nearest matching catch clause, popping call frames that cannot handle
    /// it. Returns `Some(results)` when a catch targeted the outermost
    /// frame's function label (the invocation returns those results),
    /// `None` when execution continues after a caught branch, and the
    /// original error when the exception escapes to the host.
    fn unwind(&mut self, exn: usize) -> Result<Option<Vec<Value>>, ExecFail> {
        let (tag, args) = {
            let exception = &self.store.exceptions[exn];
            (exception.tag, exception.args.clone())
        };
        loop {
            match self.catch_in_top_frame(tag, &args, exn)? {
                CatchResult::Miss => {}
                CatchResult::Caught => return Ok(None),
                CatchResult::Finished(results) => return Ok(Some(results)),
            }
            if self.frames.len() == 1 {
                return Err(ExecFail::Exception(exn));
            }
            // The top frame had no handler: abandon it (and its operand-stack
            // contribution) and continue searching in its caller.
            let frame = self.frames.pop().unwrap();
            self.stack.truncate(frame.base);
        }
    }

    /// Scan the top frame's active `try_table` scopes (labels still on the
    /// stack) for the first catch clause matching `tag`, innermost scope
    /// first, and branch to its target label with the clause's payload when
    /// one matches.
    fn catch_in_top_frame(
        &mut self,
        tag: usize,
        args: &[Value],
        exn: usize,
    ) -> Result<CatchResult, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let count = self.frames[frame_index].labels.len();
        for pos in (0..count).rev() {
            let open = self.frames[frame_index].labels[pos].open;
            let instr = self.body(frame_index).get(open).cloned();
            let clauses = match instr {
                Some(Instr::TryTable { catches, .. }) => catches,
                _ => continue,
            };
            // The clause tags are indices into this frame's tag space; the
            // thrown exception carries the tag's store cell, so resolve each
            // clause against the same instance before comparing.
            let instance = self.frames[frame_index].instance;
            let cells: Vec<Option<usize>> = clauses
                .iter()
                .map(|clause| match clause {
                    Catch::Tag { tag, .. } | Catch::TagRef { tag, .. } => self.store.instances
                        [instance]
                        .tags
                        .get(*tag as usize)
                        .copied(),
                    Catch::All { .. } | Catch::AllRef { .. } => None,
                })
                .collect();
            for (clause, cell) in clauses.iter().zip(&cells) {
                match (clause, cell) {
                    (Catch::Tag { label, .. }, Some(cell)) if *cell == tag => {
                        return self.catch_branch(pos, *label, args.to_vec());
                    }
                    (Catch::TagRef { label, .. }, Some(cell)) if *cell == tag => {
                        let mut payload = args.to_vec();
                        payload.push(Value::Ref(RefValue::Exn(exn)));
                        return self.catch_branch(pos, *label, payload);
                    }
                    (Catch::All { label }, _) => {
                        return self.catch_branch(pos, *label, Vec::new());
                    }
                    (Catch::AllRef { label }, _) => {
                        return self.catch_branch(
                            pos,
                            *label,
                            vec![Value::Ref(RefValue::Exn(exn))],
                        );
                    }
                    (Catch::Tag { .. } | Catch::TagRef { .. }, _) => {}
                }
            }
        }
        Ok(CatchResult::Miss)
    }

    /// Perform the branch a matched catch clause requests: drop the try's
    /// inner labels plus the try label itself, then deliver `payload` to the
    /// clause's target label (relative to the labels that enclosed the
    /// `try_table`), exactly like a `br` to that label.
    fn catch_branch(
        &mut self,
        pos: usize,
        label: u32,
        payload: Vec<Value>,
    ) -> Result<CatchResult, ExecFail> {
        let frame_index = self.frames.len() - 1;
        self.frames[frame_index].labels.truncate(pos);
        let target_pos = pos
            .checked_sub(1 + label as usize)
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        let target = self.frames[frame_index].labels[target_pos];
        self.stack.truncate(target.height);
        self.stack.extend(payload);
        if target_pos == 0 {
            // The clause targeted the function label: the payload is this
            // frame's results, so return from it.
            self.frames.pop();
            if self.frames.is_empty() {
                let results = self.stack.split_off(0);
                return Ok(CatchResult::Finished(results));
            }
            return Ok(CatchResult::Caught);
        }
        let map = self.map_for(frame_index);
        if target.is_loop {
            self.frames[frame_index].labels.truncate(target_pos + 1);
            self.frames[frame_index].pc = target.open + 1;
        } else {
            self.frames[frame_index].labels.truncate(target_pos);
            self.frames[frame_index].pc = map.end[target.open] + 1;
        }
        Ok(CatchResult::Caught)
    }

    /// Execute one instruction of the top frame.
    fn step(&mut self) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        if self.frames[frame_index].pc >= self.body(frame_index).len() {
            return self.finish();
        }
        let pc = self.frames[frame_index].pc;
        let instr = self.body(frame_index)[pc].clone();
        match instr {
            Instr::Unreachable => Err(ExecFail::Trap(Trap::Unreachable)),
            Instr::Nop => Ok(Ctl::Next),
            Instr::Block(_) | Instr::Loop(_) | Instr::If(_) | Instr::TryTable { .. } => {
                self.enter_structured(pc)?;
                Ok(Ctl::Settled)
            }
            Instr::Else => {
                self.exit_if()?;
                Ok(Ctl::Settled)
            }
            Instr::End => {
                self.frames[frame_index].labels.pop();
                Ok(Ctl::Next)
            }
            Instr::Br(label) => {
                if self.branch_to(label as usize)? {
                    return self.finish();
                }
                Ok(Ctl::Settled)
            }
            Instr::BrIf(label) => {
                if self.pop_i32()? != 0 {
                    if self.branch_to(label as usize)? {
                        return self.finish();
                    }
                    Ok(Ctl::Settled)
                } else {
                    Ok(Ctl::Next)
                }
            }
            Instr::BrTable { targets, default } => {
                let index = self.pop_i32()?;
                let target = if index >= 0 && (index as usize) < targets.len() {
                    targets[index as usize]
                } else {
                    default
                };
                if self.branch_to(target as usize)? {
                    return self.finish();
                }
                Ok(Ctl::Settled)
            }
            Instr::Return => self.finish(),
            Instr::Call(index) => self.do_call(index as usize),
            Instr::ReturnCall(index) => self.do_tail_call(index as usize),
            Instr::CallIndirect {
                type_index,
                table_index,
            } => self.call_indirect(type_index as usize, table_index as usize),
            Instr::ReturnCallIndirect {
                type_index,
                table_index,
            } => self.return_call_indirect(type_index as usize, table_index as usize),
            Instr::CallRef(type_index) => self.call_ref(type_index as usize),
            Instr::ReturnCallRef(type_index) => self.return_call_ref(type_index as usize),
            Instr::Drop => {
                self.pop()?;
                Ok(Ctl::Next)
            }
            Instr::Select | Instr::SelectTyped(_) => {
                let cond = self.pop_i32()?;
                let b = self.pop()?;
                let a = self.pop()?;
                self.stack.push(if cond != 0 { a } else { b });
                Ok(Ctl::Next)
            }
            Instr::LocalGet(index) => {
                let value = self.frames[frame_index].locals[index as usize];
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::LocalSet(index) => {
                let value = self.pop()?;
                self.frames[frame_index].locals[index as usize] = value;
                Ok(Ctl::Next)
            }
            Instr::LocalTee(index) => {
                let value = self.pop()?;
                self.frames[frame_index].locals[index as usize] = value;
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::GlobalGet(index) => {
                let instance = self.frames[frame_index].instance;
                let cell = self.store.instances[instance].globals[index as usize];
                let value = self.store.globals[cell];
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::GlobalSet(index) => {
                let value = self.pop()?;
                let instance = self.frames[frame_index].instance;
                let cell = self.store.instances[instance].globals[index as usize];
                self.store.globals[cell] = value;
                Ok(Ctl::Next)
            }
            Instr::TableGet(table) => {
                let frame_index = self.frames.len() - 1;
                let index = self.pop_table_addr(frame_index, table as usize)?;
                let cell = self.table_cell(frame_index, table as usize)?;
                let elements = &self.store.tables[cell].elements;
                if index >= elements.len() as u64 {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
                }
                let value = Value::Ref(elements[index as usize]);
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::TableSet(table) => {
                let value = self.pop()?;
                let frame_index = self.frames.len() - 1;
                let index = self.pop_table_addr(frame_index, table as usize)?;
                let cell = self.table_cell(frame_index, table as usize)?;
                let elements = &mut self.store.tables[cell].elements;
                if index >= elements.len() as u64 {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
                }
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("non-reference table value"));
                };
                elements[index as usize] = reference;
                Ok(Ctl::Next)
            }
            Instr::I32Const(v) => {
                self.stack.push(Value::I32(v));
                Ok(Ctl::Next)
            }
            Instr::I64Const(v) => {
                self.stack.push(Value::I64(v));
                Ok(Ctl::Next)
            }
            Instr::F32Const(bits) => {
                self.stack.push(Value::F32(bits));
                Ok(Ctl::Next)
            }
            Instr::F64Const(bits) => {
                self.stack.push(Value::F64(bits));
                Ok(Ctl::Next)
            }
            Instr::Num(op) => {
                let inputs = num_inputs(op);
                let mut operands = Vec::with_capacity(inputs);
                for _ in 0..inputs {
                    operands.push(self.pop()?);
                }
                operands.reverse();
                self.stack.push(exec_num(op, &operands)?);
                Ok(Ctl::Next)
            }
            // ---- v128 (Cut 7) ----
            Instr::V128Const(bits) => {
                self.stack.push(Value::V128(bits));
                Ok(Ctl::Next)
            }
            Instr::VecLoad {
                memory, op, offset, ..
            } => {
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
                let size = op.bytes() as u64;
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(size).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                let bytes = &self.store.memories[cell].bytes[start..start + size as usize];
                let value = load_vec(op, bytes).ok_or(ExecFail::Unsupported("simd load form"))?;
                self.stack.push(Value::V128(value));
                Ok(Ctl::Next)
            }
            Instr::VecStore { memory, offset, .. } => {
                let value = self.pop()?;
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
                let Value::V128(bits) = value else {
                    return Err(ExecFail::Unsupported("non-v128 store"));
                };
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(16).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                self.store.memories[cell].bytes[start..start + 16]
                    .copy_from_slice(&bits.to_le_bytes());
                Ok(Ctl::Next)
            }
            Instr::VecLaneLoad {
                memory,
                size,
                offset,
                lane,
                ..
            } => {
                let vector = self.pop_v128()?;
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
                let size = size as u64;
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(size).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                let mut bits = 0u64;
                for (i, &byte) in self.store.memories[cell].bytes[start..start + size as usize]
                    .iter()
                    .enumerate()
                {
                    bits |= u64::from(byte) << (8 * i);
                }
                let out = crate::simd::set_lane(vector, lane as usize, size as usize, bits);
                self.stack.push(Value::V128(out));
                Ok(Ctl::Next)
            }
            Instr::VecLaneStore {
                memory,
                size,
                offset,
                lane,
                ..
            } => {
                let vector = self.pop_v128()?;
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
                let size = size as u64;
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(size).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                let bits = crate::simd::lane_bits(vector, lane as usize, size as usize);
                for (i, byte) in self.store.memories[cell].bytes[start..start + size as usize]
                    .iter_mut()
                    .enumerate()
                {
                    *byte = (bits >> (8 * i)) as u8;
                }
                Ok(Ctl::Next)
            }
            Instr::VecShuffle(lanes) => {
                let b = self.pop_v128()?;
                let a = self.pop_v128()?;
                let mut out = 0u128;
                for (i, &lane) in lanes.iter().enumerate() {
                    let source = if lane < 16 { a } else { b };
                    let index = (lane & 15) as usize;
                    let byte = crate::simd::lane_bits(source, index, 1);
                    out = crate::simd::set_lane(out, i, 1, byte);
                }
                self.stack.push(Value::V128(out));
                Ok(Ctl::Next)
            }
            Instr::Vec(sub) => self.simd_exec(sub, None),
            Instr::VecLane { op, lane } => self.simd_exec(op, Some(lane)),
            Instr::Load {
                memory, op, offset, ..
            } => {
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
                let size = load_size(op);
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(size as u64).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                let value = read_mem(op, &self.store.memories[cell].bytes[start..start + size]);
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::Store {
                memory, op, offset, ..
            } => {
                let value = self.pop()?;
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let addr = self.pop_mem_addr(instance, memory)?;
                let size = store_size(op);
                let ea = addr
                    .checked_add(offset)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let len = self.store.memories[cell].bytes.len() as u64;
                if ea.checked_add(size as u64).is_none_or(|end| end > len) {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let start = ea as usize;
                write_mem(
                    op,
                    value,
                    &mut self.store.memories[cell].bytes[start..start + size],
                );
                Ok(Ctl::Next)
            }
            Instr::MemorySize(memory) => {
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let pages = self.store.memories[cell].pages();
                if self.store.memory_types[cell].memory64 {
                    self.stack.push(Value::I64(pages as i64));
                } else {
                    self.stack.push(Value::I32(pages as i32));
                }
                Ok(Ctl::Next)
            }
            Instr::MemoryGrow(memory) => {
                let instance = self.frames[frame_index].instance;
                let cell = self.mem_cell(instance, memory)?;
                let memory64 = self.store.memory_types[cell].memory64;
                let delta = if memory64 {
                    self.pop_i64()? as u64
                } else {
                    self.pop_i32()? as u32 as u64
                };
                let old = self.store.memories[cell].grow(delta, memory64);
                match old {
                    Some(old) if memory64 => self.stack.push(Value::I64(old as i64)),
                    Some(old) => self.stack.push(Value::I32(old as i32)),
                    None if memory64 => self.stack.push(Value::I64(-1)),
                    None => self.stack.push(Value::I32(-1)),
                }
                Ok(Ctl::Next)
            }
            Instr::MemoryInit { data_index, memory } => {
                self.memory_init(data_index as usize, frame_index, memory)
            }
            Instr::DataDrop(data_index) => {
                let instance = self.frames[frame_index].instance;
                self.store.instances[instance].data_segments[data_index as usize] = None;
                Ok(Ctl::Next)
            }
            Instr::MemoryCopy { dst, src } => self.memory_copy(frame_index, dst, src),
            Instr::MemoryFill(memory) => self.memory_fill(frame_index, memory),
            Instr::RefNull(_) => {
                self.stack.push(Value::Ref(RefValue::Null));
                Ok(Ctl::Next)
            }
            Instr::RefIsNull => {
                let value = self.pop()?;
                let is_null = matches!(value, Value::Ref(RefValue::Null));
                self.stack.push(Value::I32(i32::from(is_null)));
                Ok(Ctl::Next)
            }
            Instr::RefFunc(index) => {
                let instance = self.frames[frame_index].instance;
                let value = Value::Ref(RefValue::Func(FuncAddr {
                    instance,
                    index: index as usize,
                }));
                self.stack.push(value);
                Ok(Ctl::Next)
            }
            Instr::RefEq => {
                let b = self.pop()?;
                let a = self.pop()?;
                let equal = match (a, b) {
                    (Value::Ref(x), Value::Ref(y)) => x == y,
                    _ => return Err(ExecFail::Unsupported("ref.eq on non-reference")),
                };
                self.stack.push(Value::I32(i32::from(equal)));
                Ok(Ctl::Next)
            }
            Instr::RefAsNonNull => {
                let value = self.pop()?;
                match value {
                    Value::Ref(RefValue::Null) => Err(ExecFail::Trap(Trap::NullReference)),
                    reference => {
                        self.stack.push(reference);
                        Ok(Ctl::Next)
                    }
                }
            }
            Instr::BrOnNull(label) => {
                let value = self.pop()?;
                if matches!(value, Value::Ref(RefValue::Null)) {
                    if self.branch_to(label as usize)? {
                        return self.finish();
                    }
                    Ok(Ctl::Settled)
                } else {
                    self.stack.push(value);
                    Ok(Ctl::Next)
                }
            }
            Instr::BrOnNonNull(label) => {
                let value = self.pop()?;
                if !matches!(value, Value::Ref(RefValue::Null)) {
                    self.stack.push(value);
                    if self.branch_to(label as usize)? {
                        return self.finish();
                    }
                    Ok(Ctl::Settled)
                } else {
                    // The null is consumed: the branch is not taken and the
                    // value does not continue on the fall-through path.
                    Ok(Ctl::Next)
                }
            }
            Instr::Throw(index) => {
                // Pop the tag's arguments and raise an in-flight exception.
                let frame_index = self.frames.len() - 1;
                let instance = self.frames[frame_index].instance;
                let cell = self.store.instances[instance]
                    .tags
                    .get(index as usize)
                    .copied()
                    .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                let param_count = self.store.tags[cell].ty.params.len();
                let args = self.take_top(param_count);
                let exn = self.store.exceptions.len();
                self.store
                    .exceptions
                    .push(ExceptionInst { tag: cell, args });
                Err(ExecFail::Exception(exn))
            }
            Instr::ThrowRef => {
                let value = self.pop()?;
                match value {
                    Value::Ref(RefValue::Null) => Err(ExecFail::Trap(Trap::NullExceptionReference)),
                    Value::Ref(RefValue::Exn(exn)) => Err(ExecFail::Exception(exn)),
                    _ => Err(ExecFail::Unsupported("throw_ref on non-exception ref")),
                }
            }
            Instr::TableInit {
                element_index,
                table,
            } => self.table_init(element_index as usize, table as usize),
            Instr::ElemDrop(element_index) => {
                let instance = self.frames[frame_index].instance;
                self.store.instances[instance].element_segments[element_index as usize] = None;
                Ok(Ctl::Next)
            }
            Instr::TableCopy { dst, src } => self.table_copy(dst as usize, src as usize),
            Instr::TableGrow(table) => self.table_grow(table as usize),
            Instr::TableSize(table) => self.table_size(table as usize),
            Instr::TableFill(table) => self.table_fill(table as usize),
            // ---- GC aggregates (Cut 9 wave 3) ----
            Instr::StructNew(ty) => {
                let instance = self.frames[frame_index].instance;
                let storage = self.frame_struct_storage(instance, ty)?;
                let mut cells = Vec::with_capacity(storage.len());
                for st in storage.iter().rev() {
                    let value = self.pop()?;
                    cells.push(wrap_cell(*st, value)?);
                }
                cells.reverse();
                let id = alloc_struct(self.store, instance, ty, cells);
                self.stack.push(Value::Ref(RefValue::Struct(id)));
                Ok(Ctl::Next)
            }
            Instr::StructNewDefault(ty) => {
                let instance = self.frames[frame_index].instance;
                let storage = self.frame_struct_storage(instance, ty)?;
                let mut cells = Vec::with_capacity(storage.len());
                for st in storage {
                    cells.push(default_storage(st)?);
                }
                let id = alloc_struct(self.store, instance, ty, cells);
                self.stack.push(Value::Ref(RefValue::Struct(id)));
                Ok(Ctl::Next)
            }
            Instr::StructGet { field, .. } => self.struct_field_read(field as usize, GcRead::Plain),
            Instr::StructGetS { field, .. } => {
                self.struct_field_read(field as usize, GcRead::Signed)
            }
            Instr::StructGetU { field, .. } => {
                self.struct_field_read(field as usize, GcRead::Unsigned)
            }
            Instr::StructSet { field, .. } => {
                let value = self.pop()?;
                let reference = self.pop()?;
                let Value::Ref(reference) = reference else {
                    return Err(ExecFail::Unsupported("non-reference struct operand"));
                };
                let RefValue::Struct(id) = reference else {
                    return Err(if reference == RefValue::Null {
                        ExecFail::Trap(Trap::NullStructReference)
                    } else {
                        ExecFail::Unsupported("non-struct struct.set operand")
                    });
                };
                let storage = gc_struct_storage(self.store, id)?;
                let st = *storage
                    .get(field as usize)
                    .ok_or(ExecFail::Unsupported("struct field out of range"))?;
                let cell = wrap_cell(st, value)?;
                match &mut self.store.objects[id].data {
                    GcData::Struct(cells) => {
                        cells[field as usize] = cell;
                    }
                    _ => return Err(ExecFail::Unsupported("struct.set of non-struct")),
                }
                Ok(Ctl::Next)
            }
            Instr::ArrayNew(ty) => {
                let instance = self.frames[frame_index].instance;
                let len = self.pop_i32()?;
                let value = self.pop()?;
                let st = self.frame_array_storage(instance, ty)?;
                let cell = wrap_cell(st, value)?;
                let id = alloc_array_filled(self.store, instance, ty, len, cell)?;
                self.stack.push(Value::Ref(RefValue::Array(id)));
                Ok(Ctl::Next)
            }
            Instr::ArrayNewDefault(ty) => {
                let instance = self.frames[frame_index].instance;
                let len = self.pop_i32()?;
                let st = self.frame_array_storage(instance, ty)?;
                let cell = default_storage(st)?;
                let id = alloc_array_filled(self.store, instance, ty, len, cell)?;
                self.stack.push(Value::Ref(RefValue::Array(id)));
                Ok(Ctl::Next)
            }
            Instr::ArrayNewFixed { ty, n } => {
                let instance = self.frames[frame_index].instance;
                let st = self.frame_array_storage(instance, ty)?;
                let mut cells = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let value = self.pop()?;
                    cells.push(wrap_cell(st, value)?);
                }
                cells.reverse();
                let id = alloc_array(self.store, instance, ty, cells);
                self.stack.push(Value::Ref(RefValue::Array(id)));
                Ok(Ctl::Next)
            }
            Instr::ArrayNewData { ty, data } => {
                let instance = self.frames[frame_index].instance;
                let st = self.frame_array_storage(instance, ty)?;
                let n = self.pop_i32()? as u32 as u64;
                let src = self.pop_i32()? as u32 as u64;
                let width = u64::from(
                    storage_width(st).ok_or(ExecFail::Unsupported("array.new_data of refs"))?
                        as u32,
                );
                let instance2 = self.frames[frame_index].instance;
                // A dropped data segment behaves as empty (spec: `data.drop`
                // empties the segment, so zero-length uses still succeed).
                let bytes = self.store.instances[instance2].data_segments[data as usize]
                    .as_deref()
                    .unwrap_or(&[]);
                let total = n
                    .checked_mul(width)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                let end = src
                    .checked_add(total)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
                if end > bytes.len() as u64 {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
                }
                let mut cells = Vec::with_capacity(n as usize);
                for i in 0..n {
                    let at = (src + i * width) as usize;
                    cells.push(
                        data_element(st, bytes, at)
                            .ok_or(ExecFail::Unsupported("array.new_data element read"))?,
                    );
                }
                let id = alloc_array(self.store, instance, ty, cells);
                self.stack.push(Value::Ref(RefValue::Array(id)));
                Ok(Ctl::Next)
            }
            Instr::ArrayNewElem { ty, elem } => {
                let instance = self.frames[frame_index].instance;
                let n = self.pop_i32()? as u32 as u64;
                let src = self.pop_i32()? as u32 as u64;
                // A dropped element segment behaves as empty.
                let items = self.store.instances[instance].element_segments[elem as usize]
                    .as_deref()
                    .unwrap_or(&[]);
                let end = src
                    .checked_add(n)
                    .ok_or(ExecFail::Trap(Trap::OutOfBoundsTableAccess))?;
                if end > items.len() as u64 {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
                }
                let cells = (0..n)
                    .map(|i| Value::Ref(items[(src + i) as usize]))
                    .collect();
                let id = alloc_array(self.store, instance, ty, cells);
                self.stack.push(Value::Ref(RefValue::Array(id)));
                Ok(Ctl::Next)
            }
            Instr::ArrayGet(ty) => self.array_read(ty, GcRead::Plain),
            Instr::ArrayGetS(ty) => self.array_read(ty, GcRead::Signed),
            Instr::ArrayGetU(ty) => self.array_read(ty, GcRead::Unsigned),
            Instr::ArraySet(_ty) => {
                let value = self.pop()?;
                let index = self.pop_i32()?;
                let reference = self.pop()?;
                let Value::Ref(reference) = reference else {
                    return Err(ExecFail::Unsupported("non-reference array operand"));
                };
                let RefValue::Array(id) = reference else {
                    return Err(if reference == RefValue::Null {
                        ExecFail::Trap(Trap::NullArrayReference)
                    } else {
                        ExecFail::Unsupported("non-array array.set operand")
                    });
                };
                if (index as u32 as usize) >= gc_array_len(self.store, id)? {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
                }
                let st = gc_array_storage(self.store, id)?;
                let cell = wrap_cell(st, value)?;
                match &mut self.store.objects[id].data {
                    GcData::Array(cells) => cells[index as u32 as usize] = cell,
                    _ => return Err(ExecFail::Unsupported("array.set of non-array")),
                }
                Ok(Ctl::Next)
            }
            Instr::ArrayLen => {
                let reference = self.pop()?;
                let Value::Ref(reference) = reference else {
                    return Err(ExecFail::Unsupported("non-reference array operand"));
                };
                let RefValue::Array(id) = reference else {
                    return Err(if reference == RefValue::Null {
                        ExecFail::Trap(Trap::NullArrayReference)
                    } else {
                        ExecFail::Unsupported("array.len of non-array")
                    });
                };
                let len = match &self.store.objects[id].data {
                    GcData::Array(cells) => cells.len(),
                    _ => return Err(ExecFail::Unsupported("array.len of non-array")),
                };
                self.stack.push(Value::I32(len as i32));
                Ok(Ctl::Next)
            }
            Instr::ArrayFill(_ty) => {
                let n = self.pop_i32()? as u32 as usize;
                let value = self.pop()?;
                let start = self.pop_i32()? as u32 as usize;
                let reference = self.pop()?;
                let Value::Ref(reference) = reference else {
                    return Err(ExecFail::Unsupported("non-reference array operand"));
                };
                let RefValue::Array(id) = reference else {
                    return Err(if reference == RefValue::Null {
                        ExecFail::Trap(Trap::NullArrayReference)
                    } else {
                        ExecFail::Unsupported("array.fill of non-array")
                    });
                };
                let st = gc_array_storage(self.store, id)?;
                let cell = wrap_cell(st, value)?;
                let cells = match &mut self.store.objects[id].data {
                    GcData::Array(cells) => cells,
                    _ => return Err(ExecFail::Unsupported("array.fill of non-array")),
                };
                let Some(end) = start.checked_add(n) else {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
                };
                if end > cells.len() {
                    return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
                }
                cells[start..end].fill(cell);
                Ok(Ctl::Next)
            }
            Instr::ArrayCopy { .. } => self.array_copy(),
            Instr::ArrayInitData { data, .. } => self.array_init_data(data as usize),
            Instr::ArrayInitElem { elem, .. } => self.array_init_elem(elem as usize),
            Instr::RefTest { nullable, heap } => {
                let reference = self.pop()?;
                let Value::Ref(reference) = reference else {
                    return Err(ExecFail::Unsupported("ref.test of non-reference"));
                };
                let matches = self.ref_matches(reference, nullable, heap)?;
                self.stack.push(Value::I32(i32::from(matches)));
                Ok(Ctl::Next)
            }
            Instr::RefCast { nullable, heap } => {
                let value = self.pop()?;
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("ref.cast of non-reference"));
                };
                if self.ref_matches(reference, nullable, heap)? {
                    self.stack.push(Value::Ref(reference));
                    Ok(Ctl::Next)
                } else {
                    // The spec reports every failed cast (including casting a
                    // null to a non-nullable target) as `cast failure`.
                    Err(ExecFail::Trap(Trap::CastFailure))
                }
            }
            Instr::BrOnCast { label, to, .. } => {
                let value = self.pop()?;
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("br_on_cast of non-reference"));
                };
                let matches = self.ref_matches(reference, to.nullable, to.heap)?;
                self.stack.push(Value::Ref(reference));
                if matches {
                    if self.branch_to(label as usize)? {
                        return self.finish();
                    }
                    Ok(Ctl::Settled)
                } else {
                    Ok(Ctl::Next)
                }
            }
            Instr::BrOnCastFail { label, to, .. } => {
                let value = self.pop()?;
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("br_on_cast_fail of non-reference"));
                };
                let matches = self.ref_matches(reference, to.nullable, to.heap)?;
                self.stack.push(Value::Ref(reference));
                if matches {
                    Ok(Ctl::Next)
                } else if self.branch_to(label as usize)? {
                    self.finish()
                } else {
                    Ok(Ctl::Settled)
                }
            }
            Instr::AnyConvertExtern => {
                let value = self.pop()?;
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("any.convert_extern of non-reference"));
                };
                self.stack.push(Value::Ref(reference.unwrap_extern()));
                Ok(Ctl::Next)
            }
            Instr::ExternConvertAny => {
                let value = self.pop()?;
                let Value::Ref(reference) = value else {
                    return Err(ExecFail::Unsupported("extern.convert_any of non-reference"));
                };
                let wrapped = match reference {
                    RefValue::Null => RefValue::Null,
                    other => match ExternInner::wrap(other) {
                        Some(inner) => RefValue::Extern(inner),
                        None => {
                            return Err(ExecFail::Unsupported(
                                "extern.convert_any of unboxable reference",
                            ));
                        }
                    },
                };
                self.stack.push(Value::Ref(wrapped));
                Ok(Ctl::Next)
            }
            Instr::RefI31 => {
                let value = self.pop_i32()?;
                self.stack.push(Value::Ref(RefValue::i31(value)));
                Ok(Ctl::Next)
            }
            Instr::I31GetS => self.i31_read(true),
            Instr::I31GetU => self.i31_read(false),
        }
    }

    // ---- GC execution helpers ----

    /// The struct field storage types declared at type index `ty` of the
    /// given instance's module (cloned so no borrow outlives the call).
    fn frame_struct_storage(&self, instance: usize, ty: u32) -> Result<Vec<StorageType>, ExecFail> {
        let module = &self.store.instances[instance].module;
        match struct_field_types(module, ty) {
            Some(fields) => Ok(fields.iter().map(|field| field.ty).collect()),
            None => Err(ExecFail::Unsupported("unknown struct type")),
        }
    }

    /// The array element storage declared at type index `ty` of the given
    /// instance's module.
    fn frame_array_storage(&self, instance: usize, ty: u32) -> Result<StorageType, ExecFail> {
        let module = &self.store.instances[instance].module;
        match array_element_type(module, ty) {
            Some(field) => Ok(field.ty),
            None => Err(ExecFail::Unsupported("unknown array type")),
        }
    }

    fn pop_struct_ref(&mut self) -> Result<usize, ExecFail> {
        let value = self.pop()?;
        let Value::Ref(reference) = value else {
            return Err(ExecFail::Unsupported("non-reference struct operand"));
        };
        match reference {
            RefValue::Struct(id) => Ok(id),
            RefValue::Null => Err(ExecFail::Trap(Trap::NullStructReference)),
            _ => Err(ExecFail::Unsupported("struct operand is not a struct")),
        }
    }

    fn pop_array_ref(&mut self) -> Result<usize, ExecFail> {
        let value = self.pop()?;
        let Value::Ref(reference) = value else {
            return Err(ExecFail::Unsupported("non-reference array operand"));
        };
        match reference {
            RefValue::Array(id) => Ok(id),
            RefValue::Null => Err(ExecFail::Trap(Trap::NullArrayReference)),
            _ => Err(ExecFail::Unsupported("array operand is not an array")),
        }
    }

    /// `struct.get[_s|_u]` on the popped object.
    fn struct_field_read(&mut self, field: usize, mode: GcRead) -> Result<Ctl, ExecFail> {
        let id = self.pop_struct_ref()?;
        let storage = gc_struct_storage(self.store, id)?;
        let st = *storage
            .get(field)
            .ok_or(ExecFail::Unsupported("struct field out of range"))?;
        let cell = match &self.store.objects[id].data {
            GcData::Struct(cells) => *cells
                .get(field)
                .ok_or(ExecFail::Unsupported("struct field out of range"))?,
            _ => return Err(ExecFail::Unsupported("struct.get of non-struct")),
        };
        let out = match mode {
            GcRead::Plain => cell,
            GcRead::Signed => extend_cell(st, cell, true)?,
            GcRead::Unsigned => extend_cell(st, cell, false)?,
        };
        self.stack.push(out);
        Ok(Ctl::Next)
    }

    /// `array.get[_s|_u]`: pop the index, then the array; bounds-checked.
    fn array_read(&mut self, _ty: u32, mode: GcRead) -> Result<Ctl, ExecFail> {
        let index = self.pop_i32()? as u32 as usize;
        let id = self.pop_array_ref()?;
        let st = gc_array_storage(self.store, id)?;
        let cell = match &self.store.objects[id].data {
            GcData::Array(cells) => cells
                .get(index)
                .copied()
                .ok_or(ExecFail::Trap(Trap::OutOfBoundsArrayAccess))?,
            _ => return Err(ExecFail::Unsupported("array.get of non-array")),
        };
        let out = match mode {
            GcRead::Plain => cell,
            GcRead::Signed => extend_cell(st, cell, true)?,
            GcRead::Unsigned => extend_cell(st, cell, false)?,
        };
        self.stack.push(out);
        Ok(Ctl::Next)
    }

    fn i31_read(&mut self, signed: bool) -> Result<Ctl, ExecFail> {
        let reference = self.pop()?;
        let Value::Ref(reference) = reference else {
            return Err(ExecFail::Unsupported("non-reference i31 operand"));
        };
        match reference {
            RefValue::Null => Err(ExecFail::Trap(Trap::NullI31Reference)),
            RefValue::I31(value) => {
                // `get_s` sign-extends bit 30; `get_u` zero-extends it.
                let out = if signed { (value << 1) >> 1 } else { value };
                self.stack.push(Value::I32(out));
                Ok(Ctl::Next)
            }
            _ => Err(ExecFail::Unsupported("i31.get of non-i31")),
        }
    }

    fn array_copy(&mut self) -> Result<Ctl, ExecFail> {
        // Bottom-to-top operands: dst array, dst offset, src array, src
        // offset, length.
        let n = self.pop_i32()? as u32 as usize;
        let src_offset = self.pop_i32()? as u32 as usize;
        let src = self.pop_array_ref()?;
        let dst_offset = self.pop_i32()? as u32 as usize;
        let dst = self.pop_array_ref()?;
        let src_len = gc_array_len(self.store, src)?;
        let dst_len = gc_array_len(self.store, dst)?;
        let Some(src_end) = src_offset.checked_add(n) else {
            return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
        };
        let Some(dst_end) = dst_offset.checked_add(n) else {
            return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
        };
        if src_end > src_len || dst_end > dst_len {
            return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
        }
        // Copy through a temporary so overlapping source/destination ranges
        // behave like memmove.
        let values = match &self.store.objects[src].data {
            GcData::Array(cells) => cells[src_offset..src_end].to_vec(),
            _ => return Err(ExecFail::Unsupported("array.copy of non-array")),
        };
        match &mut self.store.objects[dst].data {
            GcData::Array(cells) => cells[dst_offset..dst_end].copy_from_slice(&values),
            _ => return Err(ExecFail::Unsupported("array.copy of non-array")),
        }
        Ok(Ctl::Next)
    }

    fn array_init_data(&mut self, data: usize) -> Result<Ctl, ExecFail> {
        // Bottom-to-top operands: array, dst offset, data offset, length.
        let n = self.pop_i32()? as u32 as u64;
        let src = self.pop_i32()? as u32 as u64;
        let dst_offset = self.pop_i32()? as u32 as usize;
        let array = self.pop_array_ref()?;
        let frame_index = self.frames.len() - 1;
        let instance = self.frames[frame_index].instance;
        let st = gc_array_storage(self.store, array)?;
        let width = u64::from(
            storage_width(st).ok_or(ExecFail::Unsupported("array.init_data of refs"))? as u32,
        );
        let len = gc_array_len(self.store, array)?;
        let dst_end = dst_offset
            .checked_add(n as usize)
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsArrayAccess))?;
        if dst_end > len {
            return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
        }
        // A dropped data segment behaves as empty (zero-length uses succeed).
        let bytes = self.store.instances[instance].data_segments[data]
            .as_deref()
            .unwrap_or(&[]);
        let total = n
            .checked_mul(width)
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
        let data_end = src
            .checked_add(total)
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
        if data_end > bytes.len() as u64 {
            return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
        }
        let cells = match &mut self.store.objects[array].data {
            GcData::Array(cells) => cells,
            _ => return Err(ExecFail::Unsupported("array.init_data of non-array")),
        };
        for i in 0..n {
            let at = (src + i * width) as usize;
            let value = data_element(st, bytes, at)
                .ok_or(ExecFail::Unsupported("array.init_data element read"))?;
            cells[dst_offset + i as usize] = wrap_cell(st, value)?;
        }
        Ok(Ctl::Next)
    }

    fn array_init_elem(&mut self, elem: usize) -> Result<Ctl, ExecFail> {
        // Bottom-to-top operands: array, dst offset, elem offset, length.
        let n = self.pop_i32()? as u32 as u64;
        let src = self.pop_i32()? as u32 as u64;
        let dst_offset = self.pop_i32()? as u32 as usize;
        let array = self.pop_array_ref()?;
        let frame_index = self.frames.len() - 1;
        let instance = self.frames[frame_index].instance;
        let len = gc_array_len(self.store, array)?;
        let dst_end = dst_offset
            .checked_add(n as usize)
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsArrayAccess))?;
        if dst_end > len {
            return Err(ExecFail::Trap(Trap::OutOfBoundsArrayAccess));
        }
        // A dropped element segment behaves as empty.
        let items = self.store.instances[instance].element_segments[elem]
            .as_deref()
            .unwrap_or(&[]);
        let src_end = src
            .checked_add(n)
            .ok_or(ExecFail::Trap(Trap::OutOfBoundsTableAccess))?;
        if src_end > items.len() as u64 {
            return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
        }
        let cells = match &mut self.store.objects[array].data {
            GcData::Array(cells) => cells,
            _ => return Err(ExecFail::Unsupported("array.init_elem of non-array")),
        };
        for i in 0..n {
            cells[dst_offset + i as usize] = Value::Ref(items[(src + i) as usize]);
        }
        Ok(Ctl::Next)
    }

    /// Whether a runtime reference matches the target reftype (`nullable`,
    /// `heap`) whose heap index is in the top frame's module type space.
    fn ref_matches(
        &self,
        reference: RefValue,
        nullable: bool,
        heap: HeapType,
    ) -> Result<bool, ExecFail> {
        if reference == RefValue::Null {
            return Ok(nullable);
        }
        let frame_index = self.frames.len() - 1;
        let frame_instance = self.frames[frame_index].instance;
        let concrete = match reference {
            RefValue::Func(addr) => match self.func_type_index(addr) {
                Some(ty) => Some((addr.instance, ty)),
                None => return Ok(runtime_abs_matches(HeapType::Func, heap)),
            },
            RefValue::Struct(id) => Some((self.store.objects[id].owner, self.store.objects[id].ty)),
            RefValue::Array(id) => Some((self.store.objects[id].owner, self.store.objects[id].ty)),
            RefValue::Host(_) => return Ok(runtime_abs_matches(HeapType::Any, heap)),
            RefValue::I31(_) => return Ok(runtime_abs_matches(HeapType::I31, heap)),
            RefValue::Extern(_) => return Ok(runtime_abs_matches(HeapType::Extern, heap)),
            RefValue::Exn(_) => return Ok(runtime_abs_matches(HeapType::Exn, heap)),
            RefValue::Null => unreachable!(),
        };
        let Some((owner, ty)) = concrete else {
            return Ok(false);
        };
        let module = &self.store.instances[owner].module;
        let composite = module
            .types
            .get(ty as usize)
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?
            .composite
            .clone();
        let matches = match heap {
            HeapType::Type(target) => {
                let owner_module = &self.store.instances[owner].module;
                let frame_module = &self.store.instances[frame_instance].module;
                if owner == frame_instance {
                    owner_module.type_is_subtype(ty, target)
                } else {
                    // A cross-module cast: the object's concrete type must be
                    // equivalent to the target, or reach it through the owner's
                    // declared supertype chain (each edge compared by
                    // equivalence against the frame module's target).
                    let mut current = ty;
                    loop {
                        if crate::module::type_indices_equivalent(
                            &frame_module.types,
                            &frame_module.rec_groups,
                            target,
                            &owner_module.types,
                            &owner_module.rec_groups,
                            current,
                        ) {
                            break true;
                        }
                        let Some(parent) = owner_module
                            .types
                            .get(current as usize)
                            .and_then(|sub| sub.supertypes.first())
                            .copied()
                        else {
                            break false;
                        };
                        current = parent;
                    }
                }
            }
            abstract_target => {
                let kind = match &composite {
                    CompositeType::Func(_) => HeapType::Func,
                    CompositeType::Struct(_) => HeapType::Struct,
                    CompositeType::Array(_) => HeapType::Array,
                };
                runtime_abs_matches(kind, abstract_target)
            }
        };
        Ok(matches)
    }

    /// Whether a resolved function target satisfies a declared module type
    /// index of the top frame's module (see [`Store::func_type_matches`]).
    fn func_type_matches(&self, module: &Module, type_index: u32, target: FuncTarget) -> bool {
        self.store.func_type_matches(module, type_index, target)
    }

    /// The type index of a function reference's declared type, within the
    /// module instance that owns the reference. Function references cannot be
    /// resolved to a declared type only when they are bound to a host
    /// function from outside the module's type space (treated as abstract
    /// `func` by the caller).
    fn func_type_index(&self, addr: FuncAddr) -> Option<u32> {
        let instance = self.store.instances.get(addr.instance)?;
        let func_imports = instance
            .module
            .imports
            .iter()
            .filter(|import| matches!(import.desc, ImportDesc::Func(_)))
            .count();
        if addr.index < func_imports {
            instance
                .module
                .imports
                .iter()
                .filter_map(|import| match import.desc {
                    ImportDesc::Func(ty) => Some(ty),
                    _ => None,
                })
                .nth(addr.index)
        } else {
            instance
                .module
                .functions
                .get(addr.index - func_imports)
                .copied()
        }
    }

    // ---- bulk memory ----

    fn memory_init(
        &mut self,
        data_index: usize,
        frame_index: usize,
        memory: u32,
    ) -> Result<Ctl, ExecFail> {
        // Operands (bottom to top): dst address, data offset, length; the
        // data offset and length are i32 in both address models.
        let len = self.pop_i32()? as u32 as usize;
        let src = self.pop_i32()? as u32 as usize;
        let instance = self.frames[frame_index].instance;
        let cell = self.mem_cell(instance, memory)?;
        let dst = self.pop_mem_addr(instance, memory)? as usize;
        let Some(segment) = self.store.instances[instance].data_segments.get(data_index) else {
            return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
        };
        let source = segment.as_deref().unwrap_or(&[]);
        let ok = dst
            .checked_add(len)
            .is_some_and(|end| end <= self.store.memories[cell].bytes.len())
            && src.checked_add(len).is_some_and(|end| end <= source.len());
        if !ok {
            return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
        }
        let bytes = &mut self.store.memories[cell].bytes;
        bytes[dst..dst + len].copy_from_slice(&source[src..src + len]);
        Ok(Ctl::Next)
    }

    fn memory_copy(
        &mut self,
        frame_index: usize,
        dst_memory: u32,
        src_memory: u32,
    ) -> Result<Ctl, ExecFail> {
        let instance = self.frames[frame_index].instance;
        // The length is the smaller of the two memories' address types.
        let len =
            if self.memory_is64(instance, dst_memory)? && self.memory_is64(instance, src_memory)? {
                self.pop_i64()? as u64
            } else {
                u64::from(self.pop_i32()? as u32)
            };
        let src = self.pop_mem_addr(instance, src_memory)?;
        let dst = self.pop_mem_addr(instance, dst_memory)?;
        let dst_cell = self.mem_cell(instance, dst_memory)?;
        let src_cell = self.mem_cell(instance, src_memory)?;
        let dst_size = self.store.memories[dst_cell].bytes.len() as u64;
        let src_size = self.store.memories[src_cell].bytes.len() as u64;
        if dst.checked_add(len).is_none_or(|end| end > dst_size)
            || src.checked_add(len).is_none_or(|end| end > src_size)
        {
            return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
        }
        let dst = dst as usize;
        let src = src as usize;
        let len = len as usize;
        if dst_cell == src_cell {
            // Copy as if through a temporary, so overlapping regions behave
            // like a memmove (Vec::copy_within does exactly that).
            let bytes = &mut self.store.memories[dst_cell].bytes;
            bytes.copy_within(src..src + len, dst);
        } else {
            let source = self.store.memories[src_cell].bytes[src..src + len].to_vec();
            self.store.memories[dst_cell].bytes[dst..dst + len].copy_from_slice(&source);
        }
        Ok(Ctl::Next)
    }

    fn memory_fill(&mut self, frame_index: usize, memory: u32) -> Result<Ctl, ExecFail> {
        let instance = self.frames[frame_index].instance;
        let cell = self.mem_cell(instance, memory)?;
        // Operands (bottom to top): dst address, byte value, length; all of
        // the address type except the value, which is i32.
        let len = self.pop_mem_addr(instance, memory)? as usize;
        let value = self.pop_i32()? as u8;
        let dst = self.pop_mem_addr(instance, memory)? as usize;
        let bytes = &mut self.store.memories[cell].bytes;
        if dst.checked_add(len).is_none_or(|end| end > bytes.len()) {
            return Err(ExecFail::Trap(Trap::OutOfBoundsMemoryAccess));
        }
        bytes[dst..dst + len].fill(value);
        Ok(Ctl::Next)
    }

    // ---- table instructions ----

    fn table_init(&mut self, element_index: usize, table: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        // Operands (bottom to top): dst index, elem offset, length; the elem
        // offset and length are i32 in both table address models.
        let len = self.pop_i32()? as u32 as u64;
        let src = self.pop_i32()? as u32 as u64;
        let dst = self.pop_table_addr(frame_index, table)?;
        let instance = self.frames[frame_index].instance;
        let cell = self.table_cell(frame_index, table)?;
        let Some(segment) = self.store.instances[instance]
            .element_segments
            .get(element_index)
        else {
            return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
        };
        let source = segment.as_deref().unwrap_or(&[]);
        let size = self.store.tables[cell].elements.len() as u64;
        let ok = dst.checked_add(len).is_some_and(|end| end <= size)
            && src
                .checked_add(len)
                .is_some_and(|end| end <= source.len() as u64);
        if !ok {
            return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
        }
        let dst = dst as usize;
        let src = src as usize;
        let len = len as usize;
        let elements = &mut self.store.tables[cell].elements;
        elements[dst..dst + len].copy_from_slice(&source[src..src + len]);
        Ok(Ctl::Next)
    }

    fn table_copy(&mut self, dst_table: usize, src_table: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        // The length is the smaller of the two tables' address types.
        let len = if self.table_is64(frame_index, dst_table)?
            && self.table_is64(frame_index, src_table)?
        {
            self.pop_i64()? as u64
        } else {
            u64::from(self.pop_i32()? as u32)
        };
        let src = self.pop_table_addr(frame_index, src_table)?;
        let dst = self.pop_table_addr(frame_index, dst_table)?;
        let dst_cell = self.table_cell(frame_index, dst_table)?;
        let src_cell = self.table_cell(frame_index, src_table)?;
        let dst_size = self.store.tables[dst_cell].elements.len() as u64;
        let src_size = self.store.tables[src_cell].elements.len() as u64;
        if dst.checked_add(len).is_none_or(|end| end > dst_size)
            || src.checked_add(len).is_none_or(|end| end > src_size)
        {
            return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
        }
        let dst = dst as usize;
        let src = src as usize;
        let len = len as usize;
        if dst_cell == src_cell {
            // Copy as if through a temporary, so overlapping regions behave
            // like a memmove (Vec::copy_within does exactly that).
            let elements = &mut self.store.tables[dst_cell].elements;
            elements.copy_within(src..src + len, dst);
        } else {
            let source = self.store.tables[src_cell].elements[src..src + len].to_vec();
            self.store.tables[dst_cell].elements[dst..dst + len].copy_from_slice(&source);
        }
        Ok(Ctl::Next)
    }

    fn table_grow(&mut self, table: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let table64 = self.table_is64(frame_index, table)?;
        let delta = if table64 {
            self.pop_i64()? as u64
        } else {
            u64::from(self.pop_i32()? as u32)
        };
        let init = self.pop()?;
        let Value::Ref(init) = init else {
            return Err(ExecFail::Unsupported("non-reference table value"));
        };
        let cell = self.table_cell(frame_index, table)?;
        let old = self.store.tables[cell].grow(delta, init, table64);
        match old {
            Some(old) if table64 => self.stack.push(Value::I64(old as i64)),
            Some(old) => self.stack.push(Value::I32(old as i32)),
            None if table64 => self.stack.push(Value::I64(-1)),
            None => self.stack.push(Value::I32(-1)),
        }
        Ok(Ctl::Next)
    }

    fn table_size(&mut self, table: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let cell = self.table_cell(frame_index, table)?;
        let size = self.store.tables[cell].elements.len() as u64;
        if self.table_is64(frame_index, table)? {
            self.stack.push(Value::I64(size as i64));
        } else {
            self.stack.push(Value::I32(size as i32));
        }
        Ok(Ctl::Next)
    }

    fn table_fill(&mut self, table: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let len = self.pop_table_addr(frame_index, table)? as usize;
        let value = self.pop()?;
        let Value::Ref(value) = value else {
            return Err(ExecFail::Unsupported("non-reference table value"));
        };
        let dst = self.pop_table_addr(frame_index, table)? as usize;
        let cell = self.table_cell(frame_index, table)?;
        let elements = &mut self.store.tables[cell].elements;
        if dst.checked_add(len).is_none_or(|end| end > elements.len()) {
            return Err(ExecFail::Trap(Trap::OutOfBoundsTableAccess));
        }
        elements[dst..dst + len].fill(value);
        Ok(Ctl::Next)
    }

    fn advance(&mut self, next: usize) {
        let frame_index = self.frames.len() - 1;
        self.frames[frame_index].pc = next;
    }

    /// Pop a v128 value (validated code only ever has one here).
    fn pop_v128(&mut self) -> Result<u128, ExecFail> {
        match self.pop()? {
            Value::V128(value) => Ok(value),
            _ => Err(ExecFail::Unsupported("expected v128 operand")),
        }
    }

    /// Execute a pure-register (or lane-immediate) v128 op. `lane` is present
    /// for the extract/replace forms.
    fn simd_exec(&mut self, sub: u16, lane: Option<u8>) -> Result<Ctl, ExecFail> {
        use crate::simd::VecSig;
        let unsupported = || ExecFail::Unsupported("simd opcode");
        // Relaxed SIMD ops take a fixed operand count and one deterministic
        // behavior each (the spec allows a set of results per op); dispatch
        // them before the category match so the ternary forms do not fall
        // into the bitselect-only `Ternop` arm.
        if crate::simd::is_relaxed(sub) {
            let arity = crate::simd::relaxed_arity(sub);
            let c = (arity == 3).then(|| self.pop_v128()).transpose()?;
            let b = (arity >= 2).then(|| self.pop_v128()).transpose()?;
            let a = self.pop_v128()?;
            let out = crate::simd::exec_relaxed(sub, a, b, c).ok_or_else(unsupported)?;
            self.stack.push(Value::V128(out));
            return Ok(Ctl::Next);
        }
        match crate::simd::sig(sub) {
            Some(VecSig::Not) => {
                let v = self.pop_v128()?;
                self.stack.push(Value::V128(crate::simd::exec_not(v)));
            }
            Some(VecSig::Unop) => {
                let v = self.pop_v128()?;
                let out = crate::simd::exec_unop(sub, v).ok_or_else(unsupported)?;
                self.stack.push(Value::V128(out));
            }
            Some(VecSig::Binop) => {
                let b = self.pop_v128()?;
                let a = self.pop_v128()?;
                let out = crate::simd::exec_binop(sub, a, b).ok_or_else(unsupported)?;
                self.stack.push(Value::V128(out));
            }
            Some(VecSig::Ternop) => {
                let c = self.pop_v128()?;
                let b = self.pop_v128()?;
                let a = self.pop_v128()?;
                self.stack
                    .push(Value::V128(crate::simd::exec_bitselect(a, b, c)));
            }
            Some(VecSig::Shift) => {
                let count = self.pop_i32()? as u32;
                let v = self.pop_v128()?;
                let out = crate::simd::exec_shift(sub, v, count).ok_or_else(unsupported)?;
                self.stack.push(Value::V128(out));
            }
            Some(VecSig::Splat(kind)) => {
                let scalar = self.pop()?;
                let bits =
                    crate::simd::scalar_to_lane_bits(scalar, kind).ok_or_else(unsupported)?;
                let out = crate::simd::exec_splat(sub, bits).ok_or_else(unsupported)?;
                self.stack.push(Value::V128(out));
            }
            Some(VecSig::ExtractS(kind) | VecSig::ExtractU(kind) | VecSig::Extract(kind)) => {
                let v = self.pop_v128()?;
                let index = lane.ok_or_else(unsupported)? as usize;
                let value = crate::simd::exec_extract(sub, v, index).ok_or_else(unsupported)?;
                let _ = kind;
                self.stack.push(value);
            }
            Some(VecSig::Replace(kind)) => {
                let scalar = self.pop()?;
                let bits =
                    crate::simd::scalar_to_lane_bits(scalar, kind).ok_or_else(unsupported)?;
                let v = self.pop_v128()?;
                let index = lane.ok_or_else(unsupported)? as usize;
                let out = crate::simd::exec_replace(sub, v, index, bits).ok_or_else(unsupported)?;
                self.stack.push(Value::V128(out));
            }
            Some(VecSig::Test) => {
                let v = self.pop_v128()?;
                let out = crate::simd::exec_any_all_true(sub, v).ok_or_else(unsupported)?;
                self.stack.push(Value::I32(out));
            }
            Some(VecSig::Bitmask) => {
                let v = self.pop_v128()?;
                let out = crate::simd::exec_bitmask(sub, v).ok_or_else(unsupported)?;
                self.stack.push(Value::I32(out));
            }
            None => return Err(unsupported()),
        }
        Ok(Ctl::Next)
    }

    /// Enter a `block`/`loop`/`if` at `pc` (which is the top frame's pc).
    fn enter_structured(&mut self, pc: usize) -> Result<(), ExecFail> {
        let frame_index = self.frames.len() - 1;
        let instr = self.body(frame_index)[pc].clone();
        let (params, arity) = self.block_counts(&instr)?;
        let height = self.stack.len().saturating_sub(params);
        match instr {
            Instr::Block(_) | Instr::TryTable { .. } => {
                self.frames[frame_index].labels.push(Label {
                    arity,
                    height,
                    is_loop: false,
                    open: pc,
                });
                self.advance(pc + 1);
            }
            Instr::Loop(_) => {
                self.frames[frame_index].labels.push(Label {
                    arity: params,
                    height,
                    is_loop: true,
                    open: pc,
                });
                self.advance(pc + 1);
            }
            Instr::If(_) => {
                let cond = self.pop_i32()?;
                let map = self.map_for(frame_index);
                if cond != 0 {
                    self.frames[frame_index].labels.push(Label {
                        arity,
                        height,
                        is_loop: false,
                        open: pc,
                    });
                    self.advance(pc + 1);
                } else if let Some(else_pc) = map.else_[pc] {
                    self.frames[frame_index].labels.push(Label {
                        arity,
                        height,
                        is_loop: false,
                        open: pc,
                    });
                    self.advance(else_pc + 1);
                } else {
                    self.advance(map.end[pc] + 1);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn exit_if(&mut self) -> Result<(), ExecFail> {
        let frame_index = self.frames.len() - 1;
        let label = self.frames[frame_index]
            .labels
            .pop()
            .ok_or(ExecFail::Trap(Trap::Unreachable))?;
        let end = self.map_for(frame_index).end[label.open];
        self.advance(end + 1);
        Ok(())
    }

    /// Branch to `label` in the top frame. Returns `true` when the target is
    /// the function label (i.e. the function must return).
    fn branch_to(&mut self, label: usize) -> Result<bool, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let (pos, is_loop, arity, height, open, is_func) = {
            let frame = &self.frames[frame_index];
            let count = frame.labels.len();
            let Some(pos) = count.checked_sub(1 + label) else {
                return Err(ExecFail::Trap(Trap::UnknownFunction));
            };
            let target = frame.labels[pos];
            (
                pos,
                target.is_loop,
                target.arity,
                target.height,
                target.open,
                pos == 0,
            )
        };
        let values = self.take_top(arity);
        self.stack.truncate(height);
        self.stack.extend(values);
        if is_func {
            return Ok(true);
        }
        let map = self.map_for(frame_index);
        let frame = &mut self.frames[frame_index];
        if is_loop {
            frame.labels.truncate(pos + 1);
            frame.pc = open + 1;
        } else {
            frame.labels.truncate(pos);
            frame.pc = map.end[open] + 1;
        }
        Ok(false)
    }

    fn map_for(&self, frame_index: usize) -> BodyMap {
        precompute(self.body(frame_index))
    }

    fn block_counts(&self, instr: &Instr) -> Result<(usize, usize), ExecFail> {
        let bt = match instr {
            Instr::Block(bt) | Instr::Loop(bt) | Instr::If(bt) => *bt,
            Instr::TryTable { blocktype, .. } => *blocktype,
            _ => return Ok((0, 0)),
        };
        match bt {
            BlockType::Empty => Ok((0, 0)),
            BlockType::Val(_) => Ok((0, 1)),
            BlockType::Type(index) => {
                let frame_index = self.frames.len() - 1;
                let instance = self.frames[frame_index].instance;
                let ty = self
                    .store
                    .instances
                    .get(instance)
                    .and_then(|i| i.module.func_at(index))
                    .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                Ok((ty.params.len(), ty.results.len()))
            }
        }
    }

    fn resolve_target(&self, address: FuncAddr) -> Result<FuncTarget, ExecFail> {
        self.store
            .instances
            .get(address.instance)
            .and_then(|i| i.funcs.get(address.index))
            .copied()
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))
    }

    /// Pop a function reference from the operand stack.
    fn pop_func_ref(&mut self) -> Result<FuncAddr, ExecFail> {
        match self.pop()? {
            Value::Ref(RefValue::Func(address)) => Ok(address),
            Value::Ref(RefValue::Null) => Err(ExecFail::Trap(Trap::NullFunctionReference)),
            _ => Err(ExecFail::Trap(Trap::NullFunctionReference)),
        }
    }

    fn call_indirect(&mut self, type_index: usize, table_index: usize) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let index = self.pop_table_addr(frame_index, table_index)?;
        let instance = self.frames[frame_index].instance;
        if self.frames.len() + 1 > self.store.instances[instance].depth_limit {
            return Err(ExecFail::Trap(Trap::CallStackExhausted));
        }
        let cell = self.table_cell(frame_index, table_index)?;
        let elements = &self.store.tables[cell].elements;
        if index >= elements.len() as u64 {
            return Err(ExecFail::Trap(Trap::UndefinedElement));
        }
        let entry = elements[index as usize];
        let RefValue::Func(address) = entry else {
            return Err(ExecFail::Trap(Trap::UninitializedElement));
        };
        let target = self.resolve_target(address)?;
        if !self.func_type_matches(
            &self.store.instances[instance].module,
            type_index as u32,
            target,
        ) {
            return Err(ExecFail::Trap(Trap::IndirectCallTypeMismatch));
        }
        self.call_target(target)
    }

    fn return_call_indirect(
        &mut self,
        type_index: usize,
        table_index: usize,
    ) -> Result<Ctl, ExecFail> {
        let frame_index = self.frames.len() - 1;
        let index = self.pop_table_addr(frame_index, table_index)?;
        let instance = self.frames[frame_index].instance;
        let cell = self.table_cell(frame_index, table_index)?;
        let elements = &self.store.tables[cell].elements;
        if index >= elements.len() as u64 {
            return Err(ExecFail::Trap(Trap::UndefinedElement));
        }
        let entry = elements[index as usize];
        let RefValue::Func(address) = entry else {
            return Err(ExecFail::Trap(Trap::UninitializedElement));
        };
        let target = self.resolve_target(address)?;
        if !self.func_type_matches(
            &self.store.instances[instance].module,
            type_index as u32,
            target,
        ) {
            return Err(ExecFail::Trap(Trap::IndirectCallTypeMismatch));
        }
        self.tail_target(target)
    }

    fn call_ref(&mut self, type_index: usize) -> Result<Ctl, ExecFail> {
        let address = self.pop_func_ref()?;
        let frame_index = self.frames.len() - 1;
        let instance = self.frames[frame_index].instance;
        if self.frames.len() + 1 > self.store.instances[instance].depth_limit {
            return Err(ExecFail::Trap(Trap::CallStackExhausted));
        }
        let target = self.resolve_target(address)?;
        if !self.func_type_matches(
            &self.store.instances[instance].module,
            type_index as u32,
            target,
        ) {
            return Err(ExecFail::Trap(Trap::IndirectCallTypeMismatch));
        }
        self.call_target(target)
    }

    fn return_call_ref(&mut self, type_index: usize) -> Result<Ctl, ExecFail> {
        let address = self.pop_func_ref()?;
        let frame_index = self.frames.len() - 1;
        let instance = self.frames[frame_index].instance;
        let target = self.resolve_target(address)?;
        if !self.func_type_matches(
            &self.store.instances[instance].module,
            type_index as u32,
            target,
        ) {
            return Err(ExecFail::Trap(Trap::IndirectCallTypeMismatch));
        }
        self.tail_target(target)
    }

    /// Run a host function called from the top frame.
    fn call_host(&mut self, id: usize) -> Result<(), ExecFail> {
        let ty = self.store.host_funcs[id].ty.clone();
        if !ty.results.is_empty() {
            return Err(ExecFail::Unsupported("host function results"));
        }
        for _ in 0..ty.params.len() {
            self.pop()?;
        }
        Ok(())
    }

    /// Call `target` from the top frame: pop its arguments, push its frame,
    /// run a type-only host body, or suspend for an external host call — and
    /// advance past the call.
    fn call_target(&mut self, target: FuncTarget) -> Result<Ctl, ExecFail> {
        let caller = self.frames.len() - 1;
        match target {
            FuncTarget::Host(id) => {
                let ty = self.store.host_funcs[id].ty.clone();
                match self.store.host_funcs[id].token {
                    Some(token) => {
                        let mut args = Vec::with_capacity(ty.params.len());
                        for _ in 0..ty.params.len() {
                            args.push(self.pop()?);
                        }
                        args.reverse();
                        self.frames[caller].pc += 1;
                        Ok(Ctl::Host { token, args })
                    }
                    None => {
                        self.call_host(id)?;
                        let caller = self.frames.len() - 1;
                        self.frames[caller].pc += 1;
                        Ok(Ctl::Settled)
                    }
                }
            }
            FuncTarget::Owned {
                instance: own,
                defined,
            } => {
                let (param_count, result_count, declared) = {
                    let module = &self.store.instances[own].module;
                    let body = module
                        .bodies
                        .get(defined)
                        .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                    let type_index = module
                        .functions
                        .get(defined)
                        .copied()
                        .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                    let signature = module
                        .func_at(type_index)
                        .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                    (
                        signature.params.len(),
                        signature.results.len(),
                        body.locals.clone(),
                    )
                };
                let mut args = Vec::with_capacity(param_count);
                for _ in 0..param_count {
                    args.push(self.pop()?);
                }
                args.reverse();
                let mut locals = args;
                for ty in &declared {
                    locals.push(default_value(*ty)?);
                }
                let base = self.stack.len();
                self.frames[caller].pc += 1;
                self.frames.push(Frame {
                    instance: own,
                    pc: 0,
                    locals,
                    base,
                    results: result_count,
                    body_index: defined,
                    labels: vec![Label {
                        arity: result_count,
                        height: base,
                        is_loop: false,
                        open: 0,
                    }],
                });
                Ok(Ctl::Settled)
            }
        }
    }

    /// Make a (direct, non-tail) call from the top frame.
    fn do_call(&mut self, index: usize) -> Result<Ctl, ExecFail> {
        let caller = self.frames.len() - 1;
        let instance = self.frames[caller].instance;
        if self.frames.len() + 1 > self.store.instances[instance].depth_limit {
            return Err(ExecFail::Trap(Trap::CallStackExhausted));
        }
        let target = *self
            .store
            .instances
            .get(instance)
            .and_then(|i| i.funcs.get(index))
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        self.call_target(target)
    }

    /// Tail-call `target`: replace the top frame in place at the same
    /// operand-stack base, so the callee's results flow to the caller of the
    /// (now replaced) frame.
    fn tail_target(&mut self, target: FuncTarget) -> Result<Ctl, ExecFail> {
        let top = self.frames.len() - 1;
        match target {
            FuncTarget::Host(id) => {
                let ty = self.store.host_funcs[id].ty.clone();
                match self.store.host_funcs[id].token {
                    Some(token) => {
                        let mut args = Vec::with_capacity(ty.params.len());
                        for _ in 0..ty.params.len() {
                            args.push(self.pop()?);
                        }
                        args.reverse();
                        // The tail-called frame ends now: its results will be
                        // the host reply, delivered to its caller (or, for the
                        // outermost frame, as the invocation's results) when
                        // the run resumes.
                        let base = self.frames[top].base;
                        self.stack.truncate(base);
                        self.frames.pop();
                        Ok(Ctl::Host { token, args })
                    }
                    None => {
                        if !ty.results.is_empty() {
                            return Err(ExecFail::Unsupported("host function results"));
                        }
                        for _ in 0..ty.params.len() {
                            self.pop()?;
                        }
                        self.finish()
                    }
                }
            }
            FuncTarget::Owned {
                instance: own,
                defined,
            } => {
                let (param_count, result_count, declared) = {
                    let module = &self.store.instances[own].module;
                    let body = module
                        .bodies
                        .get(defined)
                        .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                    let type_index = module
                        .functions
                        .get(defined)
                        .copied()
                        .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                    let signature = module
                        .func_at(type_index)
                        .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                    (
                        signature.params.len(),
                        signature.results.len(),
                        body.locals.clone(),
                    )
                };
                let mut args = Vec::with_capacity(param_count);
                for _ in 0..param_count {
                    args.push(self.pop()?);
                }
                args.reverse();
                let mut locals = args;
                for ty in &declared {
                    locals.push(default_value(*ty)?);
                }
                let base = self.frames[top].base;
                self.frames.pop();
                self.frames.push(Frame {
                    instance: own,
                    pc: 0,
                    locals,
                    base,
                    results: result_count,
                    body_index: defined,
                    labels: vec![Label {
                        arity: result_count,
                        height: base,
                        is_loop: false,
                        open: 0,
                    }],
                });
                Ok(Ctl::Settled)
            }
        }
    }

    /// Make a tail call (`return_call`) from the top frame.
    fn do_tail_call(&mut self, index: usize) -> Result<Ctl, ExecFail> {
        let top = self.frames.len() - 1;
        let instance = self.frames[top].instance;
        let target = *self
            .store
            .instances
            .get(instance)
            .and_then(|i| i.funcs.get(index))
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        self.tail_target(target)
    }

    /// Pop the top frame, transferring its results to its caller. Returns the
    /// results when the popped frame was the outermost one.
    fn finish_top(&mut self) -> Result<Option<Vec<Value>>, ExecFail> {
        let frame = self
            .frames
            .pop()
            .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
        let results = self.take_top(frame.results);
        if self.frames.is_empty() {
            return Ok(Some(results));
        }
        self.stack.truncate(frame.base);
        self.stack.extend(results);
        Ok(None)
    }

    /// `finish_top` wrapped into a [`Ctl`].
    fn finish(&mut self) -> Result<Ctl, ExecFail> {
        match self.finish_top()? {
            Some(results) => Ok(Ctl::Finished(results)),
            None => Ok(Ctl::Settled),
        }
    }

    fn take_top(&mut self, count: usize) -> Vec<Value> {
        let len = self.stack.len();
        let start = len.saturating_sub(count);
        let values = self.stack[start..].to_vec();
        self.stack.truncate(start);
        values
    }
}

fn default_value(ty: ValType) -> Result<Value, ExecFail> {
    Ok(match ty {
        ValType::I32 => Value::I32(0),
        ValType::I64 => Value::I64(0),
        ValType::F32 => Value::F32(0),
        ValType::F64 => Value::F64(0),
        ValType::V128 => Value::V128(0),
        // Non-defaultable (non-null) reference locals are validated as
        // initialized-before-use; the null placeholder below is never read by
        // a valid module.
        ValType::Ref(_) => Value::Ref(RefValue::Null),
    })
}

fn num_inputs(op: NumOp) -> usize {
    use NumOp::*;
    match op {
        I32Eqz | I64Eqz | I32Clz | I32Ctz | I32Popcnt | I64Clz | I64Ctz | I64Popcnt | F32Abs
        | F32Neg | F32Ceil | F32Floor | F32Trunc | F32Nearest | F32Sqrt | F64Abs | F64Neg
        | F64Ceil | F64Floor | F64Trunc | F64Nearest | F64Sqrt | I32WrapI64 | I32TruncF32S
        | I32TruncF32U | I32TruncF64S | I32TruncF64U | I64ExtendI32S | I64ExtendI32U
        | I64TruncF32S | I64TruncF32U | I64TruncF64S | I64TruncF64U | F32ConvertI32S
        | F32ConvertI32U | F32ConvertI64S | F32ConvertI64U | F32DemoteF64 | F64ConvertI32S
        | F64ConvertI32U | F64ConvertI64S | F64ConvertI64U | F64PromoteF32 | I32ReinterpretF32
        | I64ReinterpretF64 | F32ReinterpretI32 | F64ReinterpretI64 | I32Extend8S
        | I32Extend16S | I64Extend8S | I64Extend16S | I64Extend32S | I32TruncSatF32S
        | I32TruncSatF32U | I32TruncSatF64S | I32TruncSatF64U | I64TruncSatF32S
        | I64TruncSatF32U | I64TruncSatF64S | I64TruncSatF64U => 1,
        _ => 2,
    }
}

/// Bytes an `iN.load*` reads from memory (spec 2.4.5).
fn load_size(op: LoadOp) -> usize {
    use LoadOp::*;
    match op {
        I32 | F32 => 4,
        I64 | F64 => 8,
        I32Load8S | I32Load8U | I64Load8S | I64Load8U => 1,
        I32Load16S | I32Load16U | I64Load16S | I64Load16U => 2,
        I64Load32S | I64Load32U => 4,
    }
}

/// Materialize a v128 from a `v128.load*` memory slice.
fn load_vec(op: VecLoadOp, bytes: &[u8]) -> Option<u128> {
    use crate::simd::set_lane;
    let le = |bytes: &[u8]| -> u64 {
        let mut out = 0u64;
        for (i, &b) in bytes.iter().enumerate() {
            out |= u64::from(b) << (8 * i);
        }
        out
    };
    let mut out = 0u128;
    match op {
        VecLoadOp::V128 => out = u128::from_le_bytes(bytes.try_into().ok()?),
        VecLoadOp::I8x8S | VecLoadOp::I8x8U => {
            let signed = matches!(op, VecLoadOp::I8x8S);
            for (i, &b) in bytes.iter().enumerate() {
                let wide = if signed {
                    (b as i8) as i16 as u16 as u64
                } else {
                    u64::from(b)
                };
                out = set_lane(out, i, 2, wide);
            }
        }
        VecLoadOp::I16x4S | VecLoadOp::I16x4U => {
            let signed = matches!(op, VecLoadOp::I16x4S);
            for i in 0..4 {
                let raw = le(&bytes[2 * i..2 * i + 2]) as u16;
                let wide = if signed {
                    (raw as i16) as i32 as u32 as u64
                } else {
                    u64::from(raw)
                };
                out = set_lane(out, i, 4, wide);
            }
        }
        VecLoadOp::I32x2S | VecLoadOp::I32x2U => {
            let signed = matches!(op, VecLoadOp::I32x2S);
            for i in 0..2 {
                let raw = le(&bytes[4 * i..4 * i + 4]) as u32;
                let wide = if signed {
                    (raw as i32) as i64 as u64
                } else {
                    u64::from(raw)
                };
                out = set_lane(out, i, 8, wide);
            }
        }
        VecLoadOp::I8Splat => {
            for i in 0..16 {
                out = set_lane(out, i, 1, u64::from(bytes[0]));
            }
        }
        VecLoadOp::I16Splat => {
            let raw = le(&bytes[0..2]);
            for i in 0..8 {
                out = set_lane(out, i, 2, raw);
            }
        }
        VecLoadOp::I32Splat => {
            let raw = le(&bytes[0..4]);
            for i in 0..4 {
                out = set_lane(out, i, 4, raw);
            }
        }
        VecLoadOp::I64Splat => {
            let raw = le(&bytes[0..8]);
            for i in 0..2 {
                out = set_lane(out, i, 8, raw);
            }
        }
        VecLoadOp::I32Zero => {
            out = set_lane(0, 0, 4, le(&bytes[0..4]));
        }
        VecLoadOp::I64Zero => {
            out = set_lane(0, 0, 8, le(&bytes[0..8]));
        }
    }
    Some(out)
}

/// Bytes an `iN.store*` writes to memory.
fn store_size(op: StoreOp) -> usize {
    use StoreOp::*;
    match op {
        I32 | F32 => 4,
        I64 | F64 => 8,
        I32Store8 | I64Store8 => 1,
        I32Store16 | I64Store16 => 2,
        I64Store32 => 4,
    }
}

/// Little-endian read of `bytes` as a u64 (the caller slices exactly
/// [`load_size`] bytes; unused high bytes are zero).
fn le_u64(bytes: &[u8]) -> u64 {
    let mut value = 0u64;
    for (shift, &byte) in bytes.iter().enumerate() {
        value |= u64::from(byte) << (8 * shift);
    }
    value
}

/// Interpret the memory slice as the loaded value (with the load's sign or
/// zero extension).
fn read_mem(op: LoadOp, bytes: &[u8]) -> Value {
    use LoadOp::*;
    let raw = le_u64(bytes);
    match op {
        I32 => Value::I32(raw as u32 as i32),
        I64 => Value::I64(raw as i64),
        F32 => Value::F32(raw as u32),
        F64 => Value::F64(raw),
        I32Load8S => Value::I32((raw as u8) as i8 as i32),
        I32Load8U => Value::I32((raw as u8) as i32),
        I32Load16S => Value::I32((raw as u16) as i16 as i32),
        I32Load16U => Value::I32((raw as u16) as i32),
        I64Load8S => Value::I64((raw as u8) as i8 as i64),
        I64Load8U => Value::I64((raw as u8) as i64),
        I64Load16S => Value::I64((raw as u16) as i16 as i64),
        I64Load16U => Value::I64((raw as u16) as i64),
        I64Load32S => Value::I64((raw as u32) as i32 as i64),
        I64Load32U => Value::I64((raw as u32) as i64),
    }
}

/// Store the value's low bits, little-endian, over the memory slice (the
/// caller slices exactly `store_size(op)` bytes).
fn write_mem(_op: StoreOp, value: Value, bytes: &mut [u8]) {
    let bits = match value {
        Value::I32(v) => v as u32 as u64,
        Value::I64(v) => v as u64,
        Value::F32(bits) => bits as u64,
        Value::F64(bits) => bits,
        Value::V128(bits) => bits as u64,
        Value::Ref(_) => 0,
    };
    for (shift, byte) in bytes.iter_mut().enumerate() {
        *byte = (bits >> (8 * shift)) as u8;
    }
}

/// Whether an imported memory type is satisfied by a provided one: the
/// provider must have at least the requested minimum, and no larger a maximum
/// than the request allows (spec 4.5.2 external-type subsumption).
fn memory_matches(actual: MemType, requested: MemType) -> bool {
    actual.memory64 == requested.memory64 && limits_match(actual.limits, requested.limits)
}

/// Whether an imported table type is satisfied by a provided one: element
/// reference types must be identical (tables are invariant containers) and
/// limits subsume.
fn table_matches(actual: TableType, requested: TableType) -> bool {
    actual.table64 == requested.table64
        && actual.element == requested.element
        && limits_match(actual.limits, requested.limits)
}

fn limits_match(actual: Limits, requested: Limits) -> bool {
    actual.min >= requested.min
        && requested
            .max
            .is_none_or(|max| actual.max.is_some_and(|a| a <= max))
}

fn matches_ref(expected: RefType, actual: RefType) -> bool {
    if !expected.nullable && actual.nullable {
        return false;
    }
    match (expected.heap, actual.heap) {
        (crate::types::HeapType::Func, crate::types::HeapType::Func) => true,
        (crate::types::HeapType::Extern, crate::types::HeapType::Extern) => true,
        (crate::types::HeapType::Exn, crate::types::HeapType::Exn) => true,
        (crate::types::HeapType::Type(x), crate::types::HeapType::Type(y)) => x == y,
        // A typed function reference is a function reference.
        (crate::types::HeapType::Func, crate::types::HeapType::Type(_)) => true,
        _ => false,
    }
}

/// Whether `actual` value type fits an `expected` value type (reference
/// subtyping for refs, equality otherwise).
fn matches_val(expected: ValType, actual: ValType) -> bool {
    if expected == actual {
        return true;
    }
    match (expected, actual) {
        (ValType::Ref(e), ValType::Ref(a)) => matches_ref(e, a),
        _ => false,
    }
}

/// Evaluate a constant initializer expression (no terminating `end`).
/// `module` provides the type layout for GC aggregate constant expressions
/// (spec const-expr rules) and `func_instance` is the instance whose function
/// space a `ref.func` refers to; aggregate allocations land in `store`
/// (constant `struct.new`/`array.new` and friends).
fn eval_const(
    store: &mut Store,
    module: &Module,
    expr: &[Instr],
    globals: &[Value],
    func_instance: usize,
) -> Result<Value, ExecFail> {
    let mut stack: Vec<Value> = Vec::new();
    for instr in expr {
        match instr {
            Instr::I32Const(v) => stack.push(Value::I32(*v)),
            Instr::I64Const(v) => stack.push(Value::I64(*v)),
            Instr::F32Const(bits) => stack.push(Value::F32(*bits)),
            Instr::F64Const(bits) => stack.push(Value::F64(*bits)),
            Instr::V128Const(bits) => stack.push(Value::V128(*bits)),
            Instr::GlobalGet(index) => {
                let value = *globals
                    .get(*index as usize)
                    .ok_or(ExecFail::Trap(Trap::UnknownFunction))?;
                stack.push(value);
            }
            Instr::Num(op) => {
                let inputs = num_inputs(*op);
                let mut operands = Vec::with_capacity(inputs);
                for _ in 0..inputs {
                    operands.push(
                        stack
                            .pop()
                            .ok_or(ExecFail::Unsupported("malformed constant expression"))?,
                    );
                }
                operands.reverse();
                stack.push(exec_num(*op, &operands)?);
            }
            Instr::RefNull(_) => stack.push(Value::Ref(RefValue::Null)),
            Instr::RefFunc(index) => stack.push(Value::Ref(RefValue::Func(FuncAddr {
                instance: func_instance,
                index: *index as usize,
            }))),
            Instr::RefI31 => {
                let Value::I32(value) = stack
                    .pop()
                    .ok_or(ExecFail::Unsupported("malformed constant expression"))?
                else {
                    return Err(ExecFail::Unsupported("non-i32 ref.i31 constant"));
                };
                stack.push(Value::Ref(RefValue::i31(value)));
            }
            Instr::AnyConvertExtern => {
                let value = stack
                    .pop()
                    .ok_or(ExecFail::Unsupported("malformed constant expression"))?;
                let unwrapped = match value {
                    Value::Ref(reference) => reference.unwrap_extern(),
                    _ => return Err(ExecFail::Unsupported("non-reference internalize")),
                };
                stack.push(Value::Ref(unwrapped));
            }
            Instr::ExternConvertAny => {
                let value = stack
                    .pop()
                    .ok_or(ExecFail::Unsupported("malformed constant expression"))?;
                let wrapped = match value {
                    Value::Ref(RefValue::Null) => RefValue::Null,
                    Value::Ref(reference) => match ExternInner::wrap(reference) {
                        Some(inner) => RefValue::Extern(inner),
                        None => return Err(ExecFail::Unsupported("unboxable externalize")),
                    },
                    _ => return Err(ExecFail::Unsupported("non-reference externalize")),
                };
                stack.push(Value::Ref(wrapped));
            }
            Instr::StructNew(ty) => {
                let fields = struct_field_types(module, *ty)
                    .ok_or(ExecFail::Unsupported("unknown struct type"))?;
                let mut cells = Vec::with_capacity(fields.len());
                for field in fields.iter().rev() {
                    let value = stack
                        .pop()
                        .ok_or(ExecFail::Unsupported("malformed constant expression"))?;
                    cells.push(wrap_cell(field.ty, value)?);
                }
                cells.reverse();
                let id = alloc_struct(store, func_instance, *ty, cells);
                stack.push(Value::Ref(RefValue::Struct(id)));
            }
            Instr::StructNewDefault(ty) => {
                let fields = struct_field_types(module, *ty)
                    .ok_or(ExecFail::Unsupported("unknown struct type"))?;
                let mut cells = Vec::with_capacity(fields.len());
                for field in fields {
                    cells.push(default_storage(field.ty)?);
                }
                let id = alloc_struct(store, func_instance, *ty, cells);
                stack.push(Value::Ref(RefValue::Struct(id)));
            }
            Instr::ArrayNew(ty) => {
                let field = array_element_type(module, *ty)
                    .ok_or(ExecFail::Unsupported("unknown array type"))?;
                let len = pop_const_i32(&mut stack)?;
                let value = stack
                    .pop()
                    .ok_or(ExecFail::Unsupported("malformed constant expression"))?;
                let cell = wrap_cell(field.ty, value)?;
                let id = alloc_array_filled(store, func_instance, *ty, len, cell)?;
                stack.push(Value::Ref(RefValue::Array(id)));
            }
            Instr::ArrayNewDefault(ty) => {
                let field = array_element_type(module, *ty)
                    .ok_or(ExecFail::Unsupported("unknown array type"))?;
                let len = pop_const_i32(&mut stack)?;
                let cell = default_storage(field.ty)?;
                let id = alloc_array_filled(store, func_instance, *ty, len, cell)?;
                stack.push(Value::Ref(RefValue::Array(id)));
            }
            Instr::ArrayNewFixed { ty, n } => {
                let field = array_element_type(module, *ty)
                    .ok_or(ExecFail::Unsupported("unknown array type"))?;
                let mut cells = Vec::with_capacity(*n as usize);
                for _ in 0..*n {
                    let value = stack
                        .pop()
                        .ok_or(ExecFail::Unsupported("malformed constant expression"))?;
                    cells.push(wrap_cell(field.ty, value)?);
                }
                cells.reverse();
                let id = alloc_array(store, func_instance, *ty, cells);
                stack.push(Value::Ref(RefValue::Array(id)));
            }
            _ => return Err(ExecFail::Unsupported("constant expression")),
        }
    }
    stack
        .pop()
        .ok_or(ExecFail::Unsupported("empty constant expression"))
}

/// Pop an i32 from a constant-expression value stack.
fn pop_const_i32(stack: &mut Vec<Value>) -> Result<i32, ExecFail> {
    match stack.pop() {
        Some(Value::I32(v)) => Ok(v),
        _ => Err(ExecFail::Unsupported("malformed constant expression")),
    }
}

/// Evaluate a constant expression that must produce a reference value.
fn eval_ref_const(
    store: &mut Store,
    module: &Module,
    expr: &[Instr],
    globals: &[Value],
    func_instance: usize,
) -> Result<RefValue, InstantiateError> {
    match eval_const(store, module, expr, globals, func_instance)? {
        Value::Ref(reference) => Ok(reference),
        _ => Err(InstantiateError::Unsupported("non-reference constant")),
    }
}

// ---- GC object helpers ----

/// The declared struct fields at type index `ty` of `module`.
fn struct_field_types(module: &Module, ty: u32) -> Option<&[FieldType]> {
    module
        .types
        .get(ty as usize)
        .and_then(|sub| match &sub.composite {
            CompositeType::Struct(fields) => Some(fields.as_slice()),
            _ => None,
        })
}

/// The declared array element field at type index `ty` of `module`.
fn array_element_type(module: &Module, ty: u32) -> Option<&FieldType> {
    module
        .types
        .get(ty as usize)
        .and_then(|sub| match &sub.composite {
            CompositeType::Array(field) => Some(field),
            _ => None,
        })
}

/// The field storage of a live struct object (its own declared type's
/// fields, in order).
fn gc_struct_storage(store: &Store, object: usize) -> Result<Vec<StorageType>, ExecFail> {
    let gc_object = &store.objects[object];
    let module = &store.instances[gc_object.owner].module;
    match struct_field_types(module, gc_object.ty) {
        Some(fields) => Ok(fields.iter().map(|f| f.ty).collect()),
        None => Err(ExecFail::Unsupported("struct object layout")),
    }
}

/// The element storage of a live array object.
fn gc_array_storage(store: &Store, object: usize) -> Result<StorageType, ExecFail> {
    let gc_object = &store.objects[object];
    let module = &store.instances[gc_object.owner].module;
    match array_element_type(module, gc_object.ty) {
        Some(field) => Ok(field.ty),
        None => Err(ExecFail::Unsupported("array object layout")),
    }
}

/// The default (zero/null) value for a storage type. Non-null reference
/// storage is not defaultable (validation rejects those declarations).
fn default_storage(storage: StorageType) -> Result<Value, ExecFail> {
    Ok(match storage {
        StorageType::I8 | StorageType::I16 | StorageType::I32 => Value::I32(0),
        StorageType::I64 => Value::I64(0),
        StorageType::F32 => Value::F32(0),
        StorageType::F64 => Value::F64(0),
        StorageType::V128 => Value::V128(0),
        StorageType::Ref(reftype) if reftype.nullable => Value::Ref(RefValue::Null),
        StorageType::Ref(_) => {
            return Err(ExecFail::Unsupported("non-defaultable reference field"));
        }
    })
}

/// Store `value` into a cell of the given storage type: packed cells keep
/// the wrapped unsigned low bits, everything else keeps the value verbatim.
fn wrap_cell(storage: StorageType, value: Value) -> Result<Value, ExecFail> {
    Ok(match storage {
        StorageType::I8 => match value {
            Value::I32(v) => Value::I32((v as u8) as i32),
            _ => return Err(ExecFail::Unsupported("i8 cell write")),
        },
        StorageType::I16 => match value {
            Value::I32(v) => Value::I32((v as u16) as i32),
            _ => return Err(ExecFail::Unsupported("i16 cell write")),
        },
        StorageType::I32
        | StorageType::I64
        | StorageType::F32
        | StorageType::F64
        | StorageType::V128
        | StorageType::Ref(_) => value,
    })
}

/// Read a cell with the signed/unsigned extension of the packed `get_s`/
/// `get_u` forms. Only valid for packed cells (which validation restricts the
/// `_s`/`_u` instructions to).
fn extend_cell(storage: StorageType, cell: Value, signed: bool) -> Result<Value, ExecFail> {
    let Value::I32(bits) = cell else {
        return Err(ExecFail::Unsupported("packed read of non-i32 cell"));
    };
    let out = match (storage, signed) {
        (StorageType::I8, false) => (bits as u8) as i32,
        (StorageType::I8, true) => (bits as u8) as i8 as i32,
        (StorageType::I16, false) => (bits as u16) as i32,
        (StorageType::I16, true) => (bits as u16) as i16 as i32,
        _ => return Err(ExecFail::Unsupported("unpacked signed/unsigned read")),
    };
    Ok(Value::I32(out))
}

/// The byte width of a storage type (packed, numeric, or vector). Reference
/// storage has no byte width (refs never come from data segments).
fn storage_width(storage: StorageType) -> Option<usize> {
    match storage {
        StorageType::I8 => Some(1),
        StorageType::I16 => Some(2),
        StorageType::I32 | StorageType::F32 => Some(4),
        StorageType::I64 | StorageType::F64 => Some(8),
        StorageType::V128 => Some(16),
        StorageType::Ref(_) => None,
    }
}

/// Load one data-segment element of `storage` at byte offset `at` (little
/// endian). Packed and integer loads are unsigned; packed cells keep the
/// wrapped low bits. Returns `None` when the slice is short or the storage is
/// a reference.
fn data_element(storage: StorageType, bytes: &[u8], at: usize) -> Option<Value> {
    let width = storage_width(storage)?;
    let bytes = bytes.get(at..at + width)?;
    let mut bits = 0u64;
    for (i, &byte) in bytes.iter().enumerate() {
        bits |= u64::from(byte) << (8 * i);
    }
    Some(match storage {
        StorageType::I8 | StorageType::I16 => Value::I32(bits as u32 as i32),
        StorageType::I32 => Value::I32(bits as u32 as i32),
        StorageType::I64 => Value::I64(bits as i64),
        StorageType::F32 => Value::F32(bits as u32),
        StorageType::F64 => Value::F64(bits),
        StorageType::V128 => {
            let mut wide = 0u128;
            for (i, &byte) in bytes.iter().enumerate() {
                wide |= u128::from(byte) << (8 * i);
            }
            Value::V128(wide)
        }
        StorageType::Ref(_) => return None,
    })
}

/// Append a struct object to the pool; returns its stable id.
fn alloc_struct(store: &mut Store, owner: usize, ty: u32, cells: Vec<Value>) -> usize {
    store.objects.push(GcObject {
        owner,
        ty,
        data: GcData::Struct(cells),
    });
    store.objects.len() - 1
}

/// Append an array object to the pool; returns its stable id.
fn alloc_array(store: &mut Store, owner: usize, ty: u32, cells: Vec<Value>) -> usize {
    store.objects.push(GcObject {
        owner,
        ty,
        data: GcData::Array(cells),
    });
    store.objects.len() - 1
}

/// Append an array of `len` copies of `cell`; an oversized request traps like
/// an out-of-bounds memory access instead of aborting on allocation.
fn alloc_array_filled(
    store: &mut Store,
    owner: usize,
    ty: u32,
    len: i32,
    cell: Value,
) -> Result<usize, ExecFail> {
    let count = len as u32 as usize;
    let mut elements = Vec::new();
    elements
        .try_reserve_exact(count)
        .map_err(|_| ExecFail::Trap(Trap::OutOfBoundsMemoryAccess))?;
    elements.resize(count, cell);
    Ok(alloc_array(store, owner, ty, elements))
}

/// How a `get` reads a GC field/element cell: verbatim, or with the packed
/// sign/zero extension of `get_s`/`get_u`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GcRead {
    Plain,
    Signed,
    Unsigned,
}

/// The live length of an array object.
fn gc_array_len(store: &Store, id: usize) -> Result<usize, ExecFail> {
    match &store.objects[id].data {
        GcData::Array(cells) => Ok(cells.len()),
        _ => Err(ExecFail::Unsupported("array operand is not an array")),
    }
}

/// The abstract-heap lattice for runtime `ref.test`/`ref.cast` between an
/// actual value's (abstract) heap kind and the target heap (spec 3.3.3):
/// `eq` covers `i31`/`struct`/`array`, all of which sit under `any`. The
/// `_`-arm rejects unrelated kinds (`func`/`extern`/`exn`/bottom) and the
/// concrete `Type` form (handled before the abstract lattice is consulted).
fn runtime_abs_matches(sub: HeapType, sup: HeapType) -> bool {
    use HeapType::*;
    if sub == sup {
        return true;
    }
    match sub {
        Eq => sup == Any,
        I31 | Struct | Array => sup == Eq || sup == Any,
        _ => false,
    }
}
