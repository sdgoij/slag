//! The Vm-side JIT integration: the hook the `jit` crate installs, the
//! slow-path helper table that routes the compiled code's fallbacks back
//! into the interpreter's machinery, and the per-call context those helpers
//! operate on.
//!
//! The dependency direction is one-way (`jit` depends on `runtime`), so the
//! hook is a runtime-owned registry of function pointers: the `jit` crate
//! installs a compiled-body cache (see `jit::JitCache`) and the runtime's
//! leaf-call path (`Vm::run_jit_leaf`, in `ir.rs`) consults it before
//! interpreting a certified body.
//!
//! # ABI contracts
//!
//! - **Entry**: `extern "C" fn(frame, stack, ctx) -> u64` — `frame` points
//!   at the body's frame slots, `stack` at one-past-the-frame (the compiled
//!   body pushes above it), `ctx` at the per-call [`JitCallContext`]. The
//!   return value is the completion value's bits.
//! - **Error signaling**: the context's first byte (`pending`, offset 0) is
//!   the JIT's error flag — after every slow-path helper call the compiled
//!   code loads it and, when set, jumps to its error exit (returning
//!   `undefined`); the runtime converts the pending [`JsError`] to an `Err`.
//!   This keeps the compiled body from executing any further side effect
//!   after a throwing slow path.
//! - **Slow paths**: [`JitSlowPaths`] is `#[repr(C)]` with the same field
//!   order as `jit::JitHelpers` (each field a function pointer), so the
//!   `jit` crate can convert the table without a runtime dependency.

use std::os::raw::c_void;

use crux::Value;
use crux::value::ValueKind;
use syntax::ast::{AssignOp, BinaryOp, UpdateOp};

use crate::agent::Agent;
use crate::context::ReferenceBase;
use crate::env::EnvRecord;
use crate::ir::{CompiledBody, MEMBER_CELLS, MemberValueCell, Vm, member_reference};
use crux::error::{ErrorKind, JsError};

/// The compiled entry ABI (mirrors `jit::JitEntry`; all arguments are
/// pointers, so the `*mut u64`/`*mut c_void` spelling difference is
/// ABI-invisible).
pub type JitEntry =
    unsafe extern "C" fn(frame: *mut c_void, stack: *mut c_void, ctx: *mut c_void) -> u64;

/// The per-body compiled-code metadata the cache returns on a hit (mirrors
/// `jit::JitCompiledInfo` — `#[repr(C)]`, layout-identical).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JitCompiledInfo {
    /// The entry point (cast to `usize`).
    pub entry: usize,
    /// The body's maximum value-stack depth above the frame, in slots — the
    /// JIT's working area size.
    pub stack_usage: usize,
}

/// The registry the `jit` crate populates (see `jit::install`).
#[derive(Clone, Copy)]
pub struct JitHook {
    /// The installed cache (owned by the installer; freed by `drop_cache`).
    pub cache: *mut c_void,
    /// Look up (and compile on first use) a body. `body` points at the
    /// caller's `Rc<CompiledBody>`; `in_flight` is true while another
    /// compiled body is executing, so the cache must not evict (a running
    /// frame's entry pointer stays live). Returns a `JitCompiledInfo`
    /// pointer, or null when the body is not JIT-compilable.
    pub lookup: unsafe extern "C" fn(
        cache: *mut c_void,
        body: *const c_void,
        in_flight: bool,
    ) -> *const c_void,
    /// Free the cache (called by the Agent's drop).
    pub drop_cache: unsafe extern "C" fn(cache: *mut c_void),
    /// The slow-path helper table.
    pub helpers: *const JitSlowPaths,
}

/// The per-call leaf-inline descriptor the compiled `CallFast`/`CallFastSlot`
/// probe writes (Cut 37): the machine code reads the leaf's entry and frame
/// layout from here after the probe accepts, then calls the entry directly
/// in the caller's working buffer. `#[repr(C)]` so the compiled code reads
/// the fields at fixed offsets (`offset_of!`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LeafInlineInfo {
    /// The leaf's JIT entry (0 = the probe rejected the call site).
    pub entry: u64,
    /// The leaf's maximum value-stack depth above its frame, in slots.
    pub stack_usage: u64,
    /// The leaf's frame size (params + vars + TDZ slots; a this-less leaf
    /// by the probe's gate, so no `this` slot).
    pub frame_size: u32,
    /// The leaf's parameter count.
    pub arity: u32,
}

impl LeafInlineInfo {
    /// An empty descriptor: entry 0 makes the compiled code fall back to
    /// `call_slow`.
    pub const fn empty() -> Self {
        Self {
            entry: 0,
            stack_usage: 0,
            frame_size: 0,
            arity: 0,
        }
    }
}

/// The per-call-site leaf-call cache (Cut 39): the compiled `CallFast`/
/// `CallFastSlot` sites reuse the probe helper's verdict instead of calling
/// it every visit. The machine code trusts a record only when ALL of: the
/// step index matches `site`, the ctx's LIVE `leaf_epoch` still equals
/// `epoch` (no slow-path helper that can re-enter the interpreter has run
/// since the probe), and the callee's NaN-box upper bits + payload match
/// `callee_hi`/`callee_payload` — together those cover all 64 value bits, so
/// the identity check is exact (a polymorphic call site re-probes). The
/// probe fills `leaf_inline`; a zero `entry` caches a rejection (the site
/// falls back to `call_slow`). `#[repr(C)]` with all-scalar fields: the
/// compiled code reads the fields at fixed offsets (`offset_of!`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LeafCallSiteCache {
    /// The call-site step index this record belongs to (`u32::MAX` = none).
    pub site: u32,
    /// The leaf-eligibility epoch at probe time (see `JitCallContext::leaf_epoch`).
    pub epoch: u32,
    /// The callee's `bits >> 44` (the NaN-box prefix + tag) at probe time.
    pub callee_hi: u32,
    /// The callee's `bits & PAYLOAD_MASK` (the box address >> 4) at probe time.
    pub callee_payload: u64,
    /// The probe's verdict (see `LeafInlineInfo`).
    pub leaf_inline: LeafInlineInfo,
}

impl LeafCallSiteCache {
    /// An empty record: no site matches (`site` is `u32::MAX`), so the first
    /// visit probes.
    pub const fn empty() -> Self {
        Self {
            site: u32::MAX,
            epoch: 0,
            callee_hi: 0,
            callee_payload: 0,
            leaf_inline: LeafInlineInfo::empty(),
        }
    }
}

/// The per-call context the Vm passes to a compiled body as its `ctx`
/// argument. `pending` is offset 0 — the compiled code's error-check ABI.
#[repr(C)]
pub struct JitCallContext {
    /// Set by a slow-path helper that hit an interpreter error; the JIT
    /// checks this byte after every helper call.
    pub pending: bool,
    /// The pending error (valid when `pending`).
    pub error: Option<JsError>,
    /// The running Agent (helpers route interpreter machinery through it).
    pub agent: *mut Agent,
    /// The running Vm (member helpers need its member machinery).
    pub vm: *mut Vm,
    /// The current realm's global object (`Vm::global` resolved once per
    /// call): the compiled `LoadGlobal`/`StoreGlobal` fast paths read its
    /// live `id`/`generation` in place to validate the global-value cells —
    /// a stale ctx snapshot would miss a mutation a helper made mid-run.
    pub global_object: *mut c_void,
    /// The `Agent::global_value_cells` array base (the JIT indexes it by
    /// `name & (GLOBAL_CELLS - 1)` and reads the `#[repr(C)]` cells).
    pub global_value_cells: *mut c_void,
    /// The `Agent::member_value_cells` array base (the compiled
    /// `GetMemberName` probe indexes it by `(object_id ^ name) &
    /// (MEMBER_CELLS - 1)` and reads the `#[repr(C)]` cells).
    pub member_value_cells: *mut c_void,
    /// Whether the body's env chain is EXACTLY the global env (no
    /// intermediate envs): the compiled `LoadIdent` probe is sound only then
    /// — a named function expression's self-binding scope, a block/catch
    /// scope, a `with` object, or a module env could hold a binding of the
    /// read name that shadows the global property the cell records.
    /// Computed once per call — a certified body adds no envs mid-run (no
    /// `with`/`eval` in its own statements).
    pub clean_chain: bool,
    /// One-past-the-end of the JIT's working buffer (in bytes): the
    /// compiled leaf-call probe checks the inline leaf's frame + working
    /// area fits above the current stack top before accepting.
    pub buf_end: *mut c_void,
    /// Cut 39: the leaf-eligibility epoch — the compiled code bumps it after
    /// every slow-path helper that can re-enter the interpreter (a getter,
    /// setter, `valueOf`/`toString`, or nested call), and the compiled
    /// leaf-call cache is trusted only while `leaf_call_cache.epoch ==
    /// leaf_epoch`. A certified body's own statements never touch the Vm
    /// stacks or realm count the probe's eligibility checks, so a helper is
    /// the only way those can change mid-run.
    pub leaf_epoch: u32,
    /// Cut 39: the per-call-site leaf-call cache (see `LeafCallSiteCache`).
    pub leaf_call_cache: LeafCallSiteCache,
    /// The compiled body whose machine code is running: the step-index
    /// helpers (`create_function`/`create_arrow`/`create_function_decl`/
    /// `regexp_literal`) read their step's payload (the AST and the
    /// enclosing-chain layouts) back out of `steps[step]` instead of
    /// marshalling it across the FFI boundary. The runtime holds the `Rc`
    /// for the duration of the call, so the pointer is live.
    pub body: *const crate::ir::CompiledBody,
    /// Cut 45: a compiled body's `tail_call` helper replaced the frame (the
    /// Vm's `tail_replaced` field carries the next body). The machine code
    /// returns the placeholder value; `run_jit_body` signals the caller to
    /// loop on the new body instead of completing.
    pub tail: bool,
    /// Cut 47: the running closure's NaN-boxed bits (the Vm's
    /// `current_function`), for the compiled `TailCallSelfCheck` — the
    /// machine code compares the resolved callee against it to recognize a
    /// global-name self-tail-call at runtime (the name could have been
    /// reassigned to a different closure). `0` when no function is running
    /// (a body that can contain the check always runs with one).
    pub current_function: u64,
}

/// A direct-mapped global-value cell the compiled `LoadGlobal`/`StoreGlobal`
/// fast paths read and write in place: `name` plus the capturing
/// `(global_id, generation)` validate the cached `value` against the global
/// object's LIVE identity and generation (the generation bumps on any
/// own-property change, so a match means no mutation since the cell was
/// recorded — including one a slow-path helper performed mid-run), and
/// `slot` locates the binding's property-vector entry for the store side.
/// `#[repr(C)]` with all-scalar fields: the compiled code loads the fields
/// at fixed offsets (`offset_of!`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GlobalValueCell {
    /// The global binding's atom (the cell's own slot identity check).
    pub name: crux::AtomId,
    /// The global object's identity at capture time.
    pub global_id: u64,
    /// The global object's generation at capture time.
    pub generation: u32,
    /// The binding's property-vector slot at capture time — the compiled
    /// `StoreGlobal` fast path passes it to `set_global_slot` (the property
    /// write cannot be inlined: the vector's enum layout is runtime
    /// internal). `u32::MAX` when the slot was never resolved, which
    /// disables the store fast path (a load-only cell still validates).
    pub slot: u32,
    /// The cached value's bits.
    pub value: crux::Value,
}

impl GlobalValueCell {
    /// An empty cell: `global_id` is an impossible object id, so the JIT's
    /// validation never matches it and the read falls to the slow path.
    pub fn empty() -> Self {
        Self {
            name: 0,
            global_id: u64::MAX,
            generation: 0,
            slot: u32::MAX,
            value: crux::Value::from_bits(0),
        }
    }
}

impl crux::heap::Trace for GlobalValueCell {
    fn trace(&self, visit: &mut dyn FnMut(crux::heap::GcAny)) {
        // Defense in depth: a validated cell's value is also reachable from
        // the global object, so this only keeps a stale cell's handle alive
        // (harmless over-retention) and never under-roots.
        self.value.trace(visit);
    }
}

/// The runtime's slow-path helper table (field order mirrors
/// `jit::JitHelpers`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JitSlowPaths {
    /// Full binary-operator semantics (`apply_binary`); `op` is a
    /// `BinaryOp` discriminant.
    pub binary_slow: extern "C" fn(ctx: *mut c_void, op: u64, a: u64, b: u64) -> u64,
    /// Cut 41: the string-string `Add` fast path — the compiled `Add`
    /// checked both operands' string tags, so the rope concat runs directly
    /// (skipping `apply_binary`'s dispatch and number checks). Returns the
    /// concatenated value, or 0 when either operand is not a string (a
    /// string value's bits are never 0 — the sentinel is unreachable from
    /// the compiled path's tag check).
    pub concat_strings: extern "C" fn(ctx: *mut c_void, a: u64, b: u64) -> u64,
    /// JS relational semantics for a loop test on a non-Number; returns 1
    /// when the test holds.
    pub relational_slow: extern "C" fn(ctx: *mut c_void, op: u64, a: u64, b: u64) -> u64,
    /// The general `++`/`--` machinery on a non-Number; returns the new value.
    pub update_value_slow: extern "C" fn(ctx: *mut c_void, inc: u64, value: u64) -> u64,
    /// Full JS `ToBoolean` for a heap value; returns 1 when truthy.
    pub to_boolean_slow: extern "C" fn(ctx: *mut c_void, value: u64) -> u64,
    /// Throws the TDZ ReferenceError (reported through the context).
    pub tdz_error: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Get(o, name)`; `name` is an `AtomId`.
    pub get_member_name: extern "C" fn(ctx: *mut c_void, object: u64, name: u64) -> u64,
    /// `Get(o, key)` with a computed key.
    pub get_member_computed: extern "C" fn(ctx: *mut c_void, object: u64, key: u64) -> u64,
    /// `Set(o, name, v)` (plain assignment); returns the stored value.
    pub set_member_name: extern "C" fn(ctx: *mut c_void, object: u64, name: u64, value: u64) -> u64,
    /// `Set(o, key, v)` with a computed key; returns the stored value.
    pub set_member_computed:
        extern "C" fn(ctx: *mut c_void, object: u64, key: u64, value: u64) -> u64,
    /// The general `CallFast` (a body may contain calls — leaf bodies never
    /// do): `args` points at the JIT buffer's argument region (`argc`
    /// slots). Runs the interpreter's call machinery on the Vm's own stack.
    pub call_slow:
        extern "C" fn(ctx: *mut c_void, callee: u64, this: u64, argc: u64, args: *mut u64) -> u64,
    /// Cut 37: the compiled leaf-call probe — validates the callee (a
    /// certified, environment-free, this-less leaf whose body has compiled
    /// machine code) and that the inline frame + working area fit above the
    /// current stack top in the JIT buffer, fills the leaf's frame (the
    /// params/vars/TDZ slots above the argument region; the aliased case is
    /// the arguments themselves), and returns the leaf's JIT entry (0 = the
    /// call site falls back to `call_slow`). `args` points at the argument
    /// region's first slot; `argc` is the argument count; `site` is the call
    /// site's step index (Cut 39 — the probe records it, plus the live
    /// leaf-eligibility epoch and the callee identity, so the compiled code
    /// can skip the probe on repeat visits).
    pub leaf_call_probe:
        extern "C" fn(ctx: *mut c_void, callee: u64, args: *mut u64, argc: u64, site: u64) -> u64,
    /// Read a declared top-level `var` off the global object (`name` is an
    /// `AtomId`); returns the value.
    pub get_global: extern "C" fn(ctx: *mut c_void, name: u64) -> u64,
    /// Write a declared top-level `var`; returns the stored value.
    pub set_global: extern "C" fn(ctx: *mut c_void, name: u64, value: u64) -> u64,
    /// The compiled `StoreGlobal` fast path's property-vector write: `slot`
    /// is the binding's property-vector slot (the compiled code validated
    /// the cell's `name`/`global_id`/`generation` against the live global,
    /// so the vector entry is a writable data property of `name`). Falls
    /// back to `set_global` semantics on a shape mismatch. Returns the
    /// stored value.
    pub set_global_slot: extern "C" fn(ctx: *mut c_void, name: u64, slot: u64, value: u64) -> u64,
    /// Cut 40: the compiled `AssignMemberName` fast path's in-place
    /// property write: the compiled code validated the member value cell
    /// (object id + name + generation) and computed the compound's new
    /// value, so `write_data_property` writes the vector entry directly
    /// (mirroring the inline field) and refreshes the cell — no generation
    /// bump, no [[Set]] chain walk. Falls back to the full Set machinery on
    /// any doubt (a non-writable property, a shape change, an exotic
    /// receiver). Returns the stored value.
    pub set_member_slot: extern "C" fn(ctx: *mut c_void, object: u64, name: u64, value: u64) -> u64,
    /// The identifier read a certified body uses for an outer/global binding
    /// (`resolve_binding` + `get_value`); `name` is an `AtomId`.
    pub load_ident: extern "C" fn(ctx: *mut c_void, name: u64) -> u64,
    /// Resolve an identifier reference and push it onto the Vm's reference
    /// stack (the write path's `put_var_reference` pops it).
    pub resolve_var_ident: extern "C" fn(ctx: *mut c_void, name: u64) -> u64,
    /// `PutValue` on the reference stack's top, popped with the stored value.
    pub put_var_reference: extern "C" fn(ctx: *mut c_void, value: u64) -> u64,
    /// The identifier `++`/`--` (resolve, update, store, return the result).
    pub update_ident:
        extern "C" fn(ctx: *mut c_void, name: u64, op: u64, prefix: u64, old: u64) -> u64,
    /// The general named member assign (`o.x = v` and `o.x += v`): `op` is
    /// an `AssignOp` discriminant, `old` the cached GetValue for a compound
    /// op (ignored for `=`). Returns the stored value (the assignment's
    /// result).
    pub assign_member_name: extern "C" fn(
        ctx: *mut c_void,
        op: u64,
        object: u64,
        name: u64,
        old: u64,
        value: u64,
    ) -> u64,
    /// The general computed member assign (`o[k] = v` and `o[k] += v`);
    /// `old` as above. Returns the stored value.
    pub assign_member_computed: extern "C" fn(
        ctx: *mut c_void,
        op: u64,
        object: u64,
        key: u64,
        old: u64,
        value: u64,
    ) -> u64,
    /// The capture-context read (`LoadContextSlot`): `depth` is the static
    /// context-chain depth, `index` the binding's context slot. Returns the
    /// value (a TDZ marker throws the ReferenceError).
    pub load_context: extern "C" fn(ctx: *mut c_void, depth: u64, index: u64) -> u64,
    /// The capture-context write (`StoreContextSlot`): the TDZ and const
    /// checks, then the slot write. Returns the stored value.
    pub store_context: extern "C" fn(ctx: *mut c_void, depth: u64, index: u64, value: u64) -> u64,
    /// The first-write context store (`InitContextSlot`, depth 0, no checks).
    /// Returns the stored value.
    pub init_context: extern "C" fn(ctx: *mut c_void, index: u64, value: u64) -> u64,
    /// The capture-context `++`/`--` (`UpdateContextSlot`): read, update,
    /// store, return the old (postfix) or new (prefix) value.
    pub update_context:
        extern "C" fn(ctx: *mut c_void, depth: u64, index: u64, op: u64, prefix: u64) -> u64,
    /// The per-iteration read (`LoadPerIteration`): `depth` walks out
    /// through the enclosing per-iteration envs (0 = this loop's env),
    /// `index` the head's slot. Returns the value.
    pub load_per_iter: extern "C" fn(ctx: *mut c_void, depth: u64, index: u64) -> u64,
    /// The per-iteration write (`StorePerIteration`): the bindings are
    /// always initialized and mutable, so no checks. Returns the stored
    /// value.
    pub store_per_iter: extern "C" fn(ctx: *mut c_void, depth: u64, index: u64, value: u64) -> u64,
    /// The per-iteration `++`/`--` (`UpdatePerIteration`); returns the old
    /// (postfix) or new (prefix) value.
    pub update_per_iter:
        extern "C" fn(ctx: *mut c_void, depth: u64, index: u64, op: u64, prefix: u64) -> u64,
    /// `GetValue` of the reference stack's top (`GetVarReference`); the
    /// reference stays for the write path. Returns the value.
    pub get_var_reference: extern "C" fn(ctx: *mut c_void) -> u64,
    /// The identifier `++`/`--` through the reference machinery
    /// (`UpdateVarReference`): pops the reference, puts the updated value,
    /// returns the old (postfix) or new (prefix) value.
    pub update_var_reference:
        extern "C" fn(ctx: *mut c_void, op: u64, prefix: u64, old: u64) -> u64,
    /// The compound assign through the reference machinery
    /// (`PutVarReferenceOp`): pops the reference, puts `old op value`.
    /// Returns the new value.
    pub put_var_reference_op: extern "C" fn(ctx: *mut c_void, op: u64, old: u64, value: u64) -> u64,
    /// Drop the reference stack's top (`PopVarReference`).
    pub pop_var_reference: extern "C" fn(ctx: *mut c_void) -> u64,
    /// Create a function expression's closure (`Step::CreateFunction`):
    /// `step` is the step index into `JitCallContext::body`, whose payload
    /// (the function AST, strictness, enclosing chains) is read back out.
    /// Returns the created function value.
    pub create_function: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// Create an arrow function's closure (`Step::CreateArrow`). Returns the
    /// created function value.
    pub create_arrow: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// Instantiate a hoisted top-level function declaration
    /// (`Step::FunctionDeclInit`) and store it into its frame or
    /// capture-context slot. Returns the created function value (the step
    /// completes with no value).
    pub create_function_decl: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// `new.target` (`Step::NewTarget`): the active constructor, or
    /// *undefined* at the script level.
    pub new_target: extern "C" fn(ctx: *mut c_void) -> u64,
    /// A `RegExp` literal (`Step::RegExpLiteral`): construct a fresh RegExp
    /// object; `step` is the step index into `JitCallContext::body` (the
    /// pattern/flags `JsString`s live in the step).
    pub regexp_literal: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// Cut 45: a proper tail call (`Step::TailCallFast` and the fused
    /// global/slot forms) — mirrors the interpreter's `tail_call_shared`:
    /// an ordinary certified callee replaces the current frame on the Vm
    /// (`ctx.tail` + `Vm::tail_replaced`); anything else is a normal call
    /// whose result completes the calling body's return. `args` points at
    /// `argc` slots in the JIT buffer.
    pub tail_call: extern "C" fn(
        ctx: *mut c_void,
        callee: u64,
        this: u64,
        argc: u64,
        args: *mut u64,
        direct_eval: u64,
    ) -> u64,
    /// `Step::ArgsBase` (Cut 49, the vector call form): record the current
    /// argument-vector length as the argument boundary.
    pub args_base: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::ArgsPush`: append one value to the argument vector.
    pub args_push: extern "C" fn(ctx: *mut c_void, value: u64) -> u64,
    /// `Step::ArgsSpread`: append an iterable's elements to the argument
    /// vector (the iterator protocol).
    pub args_spread: extern "C" fn(ctx: *mut c_void, iterable: u64) -> u64,
    /// `Step::Call` (the vector form): the callee/receiver on the JIT
    /// buffer, the arguments in the Vm's vector — run the full call and
    /// return its result.
    pub call_vector:
        extern "C" fn(ctx: *mut c_void, this: u64, callee: u64, direct_eval: u64) -> u64,
    /// `Step::TailCall` (the vector form): like `tail_call`, reading the
    /// arguments from the Vm's vector instead of the JIT buffer.
    pub tail_call_vector:
        extern "C" fn(ctx: *mut c_void, this: u64, callee: u64, direct_eval: u64) -> u64,
    /// Cut 51: the vector-form self-tail-call (`Step::TailCallSelfVector`,
    /// and `TailCallSelfCheckVector`'s identity-match path): pop the
    /// argument boundary, split the Vm's argument vector, and rebind the
    /// frame in place (params from the arguments, missing params and the
    /// var/lexical/`this` slots back to their entry state). Returns 1 on
    /// success — the machine code jumps back to the body's re-entry block —
    /// and 0 with a pending error on failure (the block terminates instead
    /// of re-entering).
    pub tail_call_self_vector: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::ArrayBegin`: create a fresh array, push 0 onto the Vm's
    /// array-index stack, and return the array (the machine code pushes it
    /// onto the work stack for the element steps).
    pub array_begin: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::ArrayElement`: define `value` at the current index (the
    /// array-index stack top), bump the index, and return the array.
    pub array_element: extern "C" fn(ctx: *mut c_void, array: u64, value: u64) -> u64,
    /// `Step::ArraySpread`: define each iterable element at the current
    /// index, bumping per element, and return the array.
    pub array_spread: extern "C" fn(ctx: *mut c_void, array: u64, iterable: u64) -> u64,
    /// `Step::ArrayHole`: bump the array-index stack top (a hole skips an
    /// index; the array itself stays on the work stack, untouched).
    pub array_hole: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::ArrayEnd`: pop the index stack, set the array's `length`, and
    /// return the array.
    pub array_end: extern "C" fn(ctx: *mut c_void, array: u64) -> u64,
    /// `Step::ObjectBegin`: create a plain object with the realm's
    /// `Object.prototype` and return it (the machine code pushes it onto the
    /// work stack for the property steps).
    pub object_begin: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::ObjectInitName`: define an own data property (with the
    /// `__proto__` setter special case and name inference).
    pub object_init_name: extern "C" fn(
        ctx: *mut c_void,
        object: u64,
        name: u64,
        set_name: u64,
        shorthand: u64,
        value: u64,
    ) -> u64,
    /// `Step::ObjectInitComputed`: define an own data property under a
    /// computed (already-converted) key.
    pub object_init_computed:
        extern "C" fn(ctx: *mut c_void, object: u64, key: u64, set_name: u64, value: u64) -> u64,
    /// `Step::ObjectKeyToPropertyKey`: ToPropertyKey the top value, returning
    /// the converted String/Symbol value.
    pub object_key_to_property_key: extern "C" fn(ctx: *mut c_void, key: u64) -> u64,
    /// `Step::ObjectMethodName`/`ObjectMethodComputed`: define a method
    /// (instantiate + make-method + name + define); the function payload is
    /// read back from the running body at `step`.
    pub object_method_name: extern "C" fn(ctx: *mut c_void, object: u64, step: u64) -> u64,
    pub object_method_computed:
        extern "C" fn(ctx: *mut c_void, object: u64, key: u64, step: u64) -> u64,
    /// `Step::ObjectAccessorName`/`ObjectAccessorComputed`: define a get/set
    /// accessor; the get/param/body payload is read back from the running
    /// body at `step`.
    pub object_accessor_name: extern "C" fn(ctx: *mut c_void, object: u64, step: u64) -> u64,
    pub object_accessor_computed:
        extern "C" fn(ctx: *mut c_void, object: u64, key: u64, step: u64) -> u64,
    /// `Step::ObjectSpread`: copy the source's own enumerable properties.
    pub object_spread: extern "C" fn(ctx: *mut c_void, object: u64, from: u64) -> u64,
    /// `Step::PushStr`: push a string literal — the `JsString` payload is
    /// read back from the running body at `step` and wrapped in a value.
    pub push_str: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// `Step::ConcatStr`: ToString the top value and append its units to the
    /// accumulator below it (the template-literal flatten concat).
    pub concat_str: extern "C" fn(ctx: *mut c_void, value: u64, acc: u64) -> u64,
    /// `Step::ConcatStrConst`: append a string-literal constant's units to
    /// the accumulator; the `JsString` payload is read back at `step`.
    pub concat_str_const: extern "C" fn(ctx: *mut c_void, acc: u64, step: u64) -> u64,
    /// `Step::Push` with a heap constant (a plain string/bigint literal —
    /// `compile_literal` emits `Push(Value::String(...))`, only templates use
    /// `PushStr`): return the payload's bits (the step holds the strong ref,
    /// mirroring the interpreter's `stack.push(*value)`).
    pub push_const: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// A register body's heap constant (`LeafOp::LoadConst`/`BinConst` or a
    /// member op's `RegOperand::Const`): read the value out of the running
    /// body's register op at `(step, op)` and return its bits. `field`
    /// selects the const-bearing field of the op (0 = the op's own Value,
    /// 1 = `StoreMemberName.value`, 2 = `GetMemberComputed(.Local).key`,
    /// 3/4 = `StoreMemberComputed.key/value`).
    pub load_const: extern "C" fn(ctx: *mut c_void, step: u64, op: u64, field: u64) -> u64,
}

/// The runtime's slow-path table, installed into every `JitHook`.
pub static JIT_SLOW_PATHS: JitSlowPaths = JitSlowPaths {
    binary_slow,
    concat_strings,
    relational_slow,
    update_value_slow,
    to_boolean_slow,
    tdz_error,
    get_member_name,
    get_member_computed,
    set_member_name,
    set_member_computed,
    call_slow,
    leaf_call_probe,
    get_global,
    set_global,
    set_global_slot,
    load_ident,
    resolve_var_ident,
    put_var_reference,
    update_ident,
    assign_member_name,
    assign_member_computed,
    set_member_slot,
    load_context,
    store_context,
    init_context,
    update_context,
    load_per_iter,
    store_per_iter,
    update_per_iter,
    get_var_reference,
    update_var_reference,
    put_var_reference_op,
    pop_var_reference,
    create_function,
    create_arrow,
    create_function_decl,
    new_target,
    regexp_literal,
    tail_call,
    args_base,
    args_push,
    args_spread,
    call_vector,
    tail_call_vector,
    tail_call_self_vector,
    array_begin,
    array_element,
    array_spread,
    array_hole,
    array_end,
    object_begin,
    object_init_name,
    object_init_computed,
    object_key_to_property_key,
    object_method_name,
    object_method_computed,
    object_accessor_name,
    object_accessor_computed,
    object_spread,
    push_str,
    concat_str,
    concat_str_const,
    push_const,
    load_const,
};

/// The slack (in slots) reserved above a compiled body's working area on the
/// value stack: the member helpers push their stored value once per call, and
/// the JIT's own usage is bounded by `JitCompiledInfo::stack_usage`.
pub const JIT_STACK_SLACK: usize = 16;

/// The maximum number of JIT frames nested on the native stack. Each runs
/// with its own private frame/working buffer (a stack array up to
/// `INLINE_JIT_BUF` slots in `Vm::run_jit_leaf`), so unbounded JIT nesting
/// would consume native stack faster than the interpreter; deeper recursion
/// falls back to the interpreter.
pub const MAX_JIT_DEPTH: usize = 128;

/// The JIT's per-call frame/working buffer fits a stack array up to this
/// many slots (512 bytes); larger bodies spill to a per-call heap Vec. Most
/// certified bodies are far smaller, so the hot path avoids the allocation.
pub(crate) const INLINE_JIT_BUF: usize = 64;

/// The `BinaryOp` variants in declaration order (a fieldless enum's
/// discriminant is its index — guaranteed by the language).
const BINARY_OPS: [BinaryOp; 22] = [
    BinaryOp::Exp,
    BinaryOp::Mul,
    BinaryOp::Div,
    BinaryOp::Rem,
    BinaryOp::Add,
    BinaryOp::Sub,
    BinaryOp::LeftShift,
    BinaryOp::RightShift,
    BinaryOp::UnsignedRightShift,
    BinaryOp::LessThan,
    BinaryOp::GreaterThan,
    BinaryOp::LessEqual,
    BinaryOp::GreaterEqual,
    BinaryOp::In,
    BinaryOp::Instanceof,
    BinaryOp::Equal,
    BinaryOp::NotEqual,
    BinaryOp::StrictEqual,
    BinaryOp::StrictNotEqual,
    BinaryOp::BitAnd,
    BinaryOp::BitXor,
    BinaryOp::BitOr,
];

const UPDATE_OPS: [UpdateOp; 2] = [UpdateOp::Increment, UpdateOp::Decrement];

/// The `AssignOp` variants in declaration order (a fieldless enum's
/// discriminant is its index).
const ASSIGN_OPS: [AssignOp; 16] = [
    AssignOp::Assign,
    AssignOp::AddAssign,
    AssignOp::SubAssign,
    AssignOp::MulAssign,
    AssignOp::DivAssign,
    AssignOp::RemAssign,
    AssignOp::ExpAssign,
    AssignOp::LeftShiftAssign,
    AssignOp::RightShiftAssign,
    AssignOp::UnsignedRightShiftAssign,
    AssignOp::BitAndAssign,
    AssignOp::BitXorAssign,
    AssignOp::BitOrAssign,
    AssignOp::AndAssign,
    AssignOp::OrAssign,
    AssignOp::NullishAssign,
];

unsafe fn ctx_of(ctx: *mut c_void) -> &'static mut JitCallContext {
    // SAFETY: the Vm passes `&mut JitCallContext` on its stack for the
    // duration of the (synchronous) compiled call.
    unsafe { &mut *(ctx as *mut JitCallContext) }
}

/// Report an interpreter error through the per-call context and return the
/// placeholder value the compiled code discards.
fn slow_error(ctx: &mut JitCallContext, error: JsError) -> u64 {
    ctx.error = Some(error);
    ctx.pending = true;
    Value::Undefined.bits()
}

extern "C" fn binary_slow(ctx: *mut c_void, op: u64, a: u64, b: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let op = BINARY_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(BinaryOp::Add);
    match crate::expr::apply_binary(agent, op, &Value::from_bits(a), &Value::from_bits(b)) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn concat_strings(_ctx: *mut c_void, a: u64, b: u64) -> u64 {
    // The compiled `Add` fast path checked both operands' string tags, so
    // both are strings and the rope concat cannot throw. The 0 sentinel is
    // defense in depth for a non-string operand (unreachable from the
    // compiled path — a string value's bits are never 0).
    let a = Value::from_bits(a);
    let b = Value::from_bits(b);
    match (a.as_string(), b.as_string()) {
        (Some(a), Some(b)) => Value::String(crux::string::JsString::concat(&a, &b)).bits(),
        _ => 0,
    }
}

extern "C" fn relational_slow(ctx: *mut c_void, op: u64, a: u64, b: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let op = BINARY_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(BinaryOp::LessThan);
    match crate::expr::apply_binary(agent, op, &Value::from_bits(a), &Value::from_bits(b)) {
        Ok(value) => crux::convert::to_boolean(&value) as u64,
        Err(error) => {
            slow_error(ctx, error);
            0
        }
    }
}

extern "C" fn update_value_slow(ctx: *mut c_void, inc: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let inc = UPDATE_OPS
        .get(inc as usize)
        .copied()
        .unwrap_or(UpdateOp::Increment);
    match crate::ir::update_value(agent, &inc, &Value::from_bits(value)) {
        Ok((_, new)) => new.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn to_boolean_slow(_ctx: *mut c_void, value: u64) -> u64 {
    // `ToBoolean` cannot throw.
    crux::convert::to_boolean(&Value::from_bits(value)) as u64
}

extern "C" fn tdz_error(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    slow_error(
        ctx,
        JsError::new(
            ErrorKind::ReferenceError,
            "Cannot access a binding before initialization".into(),
        ),
    )
}

extern "C" fn get_member_name(ctx: *mut c_void, object: u64, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let object = Value::from_bits(object);
    match vm.get_member_name(agent, object, name as crux::AtomId) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn get_member_computed(ctx: *mut c_void, object: u64, key: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let object = Value::from_bits(object);
    let key = Value::from_bits(key);
    match vm.get_member_computed(agent, object, key) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn set_member_name(ctx: *mut c_void, object: u64, name: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let object = Value::from_bits(object);
    let value = Value::from_bits(value);
    if object.is_undefined() || object.is_null() {
        return slow_error(
            ctx,
            JsError::new(ErrorKind::TypeError, "Cannot set properties of null".into()),
        );
    }
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.assign_member(
        agent,
        object,
        crate::ir::PropertyKeyName::Name(name as crux::AtomId),
        None,
        value,
        syntax::ast::AssignOp::Assign,
    ) {
        // `assign_member` pushed the result (the assignment's value); pop it
        // back so the interpreter's value stack stays balanced across the
        // JIT body's helpers.
        Ok(()) => match vm.stack.pop() {
            Some(result) => result.bits(),
            None => value.bits(),
        },
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn set_member_computed(ctx: *mut c_void, object: u64, key: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let object = Value::from_bits(object);
    let key = Value::from_bits(key);
    let value = Value::from_bits(value);
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.assign_computed_plain(agent, object, key, value) {
        Ok(()) => match vm.stack.pop() {
            Some(result) => result.bits(),
            None => value.bits(),
        },
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn call_slow(
    ctx: *mut c_void,
    callee: u64,
    this: u64,
    argc: u64,
    args: *mut u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let argc = argc as usize;
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let entry_len = vm.stack.len();
    vm.stack.push(Value::from_bits(this));
    vm.stack.push(Value::from_bits(callee));
    // The arguments are pushed straight from the JIT buffer (no copy): the
    // buffer is only written by the machine code, which is suspended for
    // the duration of this synchronous helper, so it is stable even while
    // `vm.stack` reallocates.
    for i in 0..argc {
        // SAFETY: the JIT passes a pointer into its own (live) stack buffer
        // with `argc` slots.
        vm.stack.push(Value::from_bits(unsafe { *args.add(i) }));
    }
    match vm.do_call_fast(agent, argc, false) {
        Ok(()) => {
            // `do_call_fast` replaced `[this, callee, args]` with the result.
            let result = match vm.stack.pop() {
                Some(value) => value,
                None => {
                    vm.stack.truncate(entry_len);
                    return slow_error(
                        ctx,
                        JsError::new(
                            ErrorKind::TypeError,
                            "the JIT call produced no result".into(),
                        ),
                    );
                }
            };
            debug_assert_eq!(vm.stack.len(), entry_len);
            result.bits()
        }
        Err(error) => {
            vm.stack.truncate(entry_len);
            slow_error(ctx, error)
        }
    }
}

extern "C" fn leaf_call_probe(
    ctx: *mut c_void,
    callee: u64,
    args: *mut u64,
    argc: u64,
    site: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    // Record the cache identity up front, before any rejection: the compiled
    // code reuses this record only when its step index, the live leaf epoch,
    // and the callee's full NaN-box identity all match — so a cached zero
    // entry skips the probe for a stable rejection, and a stale record is
    // simply re-probed.
    ctx.leaf_call_cache = LeafCallSiteCache {
        site: site as u32,
        epoch: ctx.leaf_epoch,
        callee_hi: (callee >> 44) as u32,
        callee_payload: callee & crux::PAYLOAD_MASK,
        leaf_inline: LeafInlineInfo::empty(),
    };
    let callee = Value::from_bits(callee);
    // The eligibility mirrors `fast_call_core`'s leaf gate (the compiled
    // call site is inside a certified body, whose own stacks are the ones
    // `can_inline_leaf` checks), plus the inline-specific restrictions: the
    // leaf must not read its environment (the body_context/lexical_env swap
    // `run_jit_leaf` performs around the run cannot be split across the
    // helper/machine-code boundary) and must have no `this` slot (the
    // machine code cannot bind `this` into the frame).
    if !vm.can_inline_leaf() || agent.realm_count.get() != 1 {
        return 0;
    }
    let ValueKind::Function(function) = callee.kind() else {
        return 0;
    };
    if !matches!(function.kind, crux::function::FunctionKind::EcmaScript) {
        return 0;
    }
    let Some(entry) = agent.leaf_lookup(function.id()) else {
        return 0;
    };
    let ir = entry.ir.clone();
    if ir.leaf_uses_env {
        return 0;
    }
    let Some(scope) = ir.scope.as_ref() else {
        return 0;
    };
    if scope.this_slot.is_some() {
        return 0;
    }
    // The leaf must have compiled machine code (compiling on first use); a
    // body without compiled code falls back to `call_slow`, whose
    // interpreter leaf-inline path handles it. The in-flight flag keeps the
    // cache from evicting a frame running right now.
    let Some(hook) = agent.jit_hook else {
        return 0;
    };
    let info_ptr = crate::jit::lookup_info(hook, &ir, agent.jit_depth > 0);
    if info_ptr.is_null() {
        return 0;
    }
    let compiled = unsafe { &*info_ptr };
    // The inline frame + working area must fit above the argument region's
    // top in the caller's working buffer: the aliased case (the frame IS
    // the arguments) needs only the working area; the built frame adds its
    // frame_size slots on top of the args.
    let argc = argc as usize;
    let aliased = scope.frame_size == scope.arity && argc >= scope.frame_size;
    let args_top = (args as usize) + argc * 8;
    let needed = (if aliased { 0 } else { scope.frame_size }) + compiled.stack_usage;
    if args_top + needed * 8 > ctx.buf_end as usize {
        return 0;
    }
    // Fill the leaf's frame above the arguments (the aliased case is the
    // arguments themselves — no fill; missing arguments stay `undefined`,
    // var slots `undefined`, lexical slots the uninitialized marker). The
    // buffer is only written by the machine code, which is suspended for
    // the duration of this synchronous helper.
    if !aliased {
        let frame = args_top as *mut u64;
        for slot in 0..scope.frame_size {
            let value = if slot < scope.arity {
                if slot < argc {
                    // SAFETY: the JIT passes a pointer into its own (live)
                    // stack buffer with `argc` slots.
                    unsafe { *args.add(slot) }
                } else {
                    Value::Undefined.bits()
                }
            } else if scope.tdz_store.get(slot).copied().unwrap_or(false) {
                Value::uninitialized().bits()
            } else {
                Value::Undefined.bits()
            };
            // SAFETY: the room check above guarantees `frame_size` slots
            // fit past the argument region's top.
            unsafe { *frame.add(slot) = value };
        }
    }
    ctx.leaf_call_cache.leaf_inline = LeafInlineInfo {
        entry: compiled.entry as u64,
        stack_usage: compiled.stack_usage as u64,
        frame_size: scope.frame_size as u32,
        arity: scope.arity as u32,
    };
    compiled.entry as u64
}

extern "C" fn get_global(ctx: *mut c_void, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.load_global_value(agent, name as crux::AtomId) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn set_global(ctx: *mut c_void, name: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let value = Value::from_bits(value);
    match vm.store_global_value(agent, name as crux::AtomId, value) {
        Ok(()) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn set_global_slot(ctx: *mut c_void, name: u64, slot: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let value = Value::from_bits(value);
    // The compiled `StoreGlobal` fast path validated the cell (same global
    // identity, live generation, resolved slot), so the property at `slot`
    // is a writable data property of `name`. Defense in depth: any shape
    // mismatch falls back to the full machinery (which also mirrors the
    // cell, keeping the load fast path warm).
    let global = unsafe { &*ctx.global_object.cast::<crux::object::JsObject>() };
    let hit = {
        let mut props = global.properties.borrow_mut();
        if let Some((key, property)) = props.get_mut(slot as usize)
            && *key == crux::property::PropertyKey::String(name as crux::AtomId)
            && let crux::object::PropertyKind::Data {
                writable: true,
                value: cell,
            } = &mut property.kind
        {
            *cell = value;
            true
        } else {
            false
        }
    };
    if hit {
        return value.bits();
    }
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.store_global_value(agent, name as crux::AtomId, value) {
        Ok(()) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn load_ident(ctx: *mut c_void, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let name_atom = name as crux::AtomId;
    let name_string = crux::lookup(name_atom);
    let reference = match crate::context::resolve_binding(agent, &name_string, vm.strict) {
        Ok(reference) => reference,
        Err(error) => return slow_error(ctx, error),
    };
    let value = match crate::context::get_value(agent, &reference) {
        Ok(value) => value,
        Err(error) => return slow_error(ctx, error),
    };
    // Warm the JIT's global fast cell when the resolved binding is a global
    // OBJECT-record (var/function/undeclared) data property — the compiled
    // `LoadIdent` probe then serves the next read as a native load. A
    // DECLARATIVE-record binding (a top-level `let`/`const`/`class`) or any
    // other env never warms: the probe validates only the cell's name and
    // the global object's version, which a declarative shadow does not
    // disturb. Best-effort — a name whose shape does not fit (an accessor,
    // an absent property) stays missing and the resolve path keeps running.
    if let ReferenceBase::Environment(env) = &reference.base
        && let EnvRecord::Global(_) = &**env
        && !env.has_lexical_declaration(&name_string)
    {
        vm.warm_global_cell(agent, name_atom, value);
    }
    value.bits()
}

extern "C" fn resolve_var_ident(ctx: *mut c_void, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match crate::context::resolve_binding(agent, &crux::lookup(name as crux::AtomId), vm.strict) {
        Ok(reference) => {
            vm.var_ref_stack.push(reference);
            0
        }
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn put_var_reference(ctx: *mut c_void, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let value = Value::from_bits(value);
    let reference = match vm.var_ref_stack.pop() {
        Some(reference) => reference,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::SyntaxError,
                    "PutVarReference without a resolution".into(),
                ),
            );
        }
    };
    match crate::context::put_value(agent, &reference, value) {
        Ok(()) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn update_ident(ctx: *mut c_void, name: u64, op: u64, prefix: u64, old: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let old = Value::from_bits(old);
    let op = UPDATE_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(UpdateOp::Increment);
    let prefix = prefix != 0;
    let (old_numeric, new) = match crate::ir::update_value(agent, &op, &old) {
        Ok(result) => result,
        Err(error) => return slow_error(ctx, error),
    };
    let reference = match crate::context::resolve_binding(
        agent,
        &crux::lookup(name as crux::AtomId),
        vm.strict,
    ) {
        Ok(reference) => reference,
        Err(error) => return slow_error(ctx, error),
    };
    match crate::context::put_value(agent, &reference, new) {
        Ok(()) => (if prefix { new } else { old_numeric }).bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn assign_member_name(
    ctx: *mut c_void,
    op: u64,
    object: u64,
    name: u64,
    old: u64,
    value: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let object = Value::from_bits(object);
    let value = Value::from_bits(value);
    let op = ASSIGN_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(AssignOp::Assign);
    if crate::ir::is_nullish(&object) {
        return slow_error(
            ctx,
            crate::ir::nullish_error("Cannot set properties of null"),
        );
    }
    let old = if crate::ir::is_compound_assign(&op) {
        Some(Value::from_bits(old))
    } else {
        None
    };
    match vm.assign_member(
        agent,
        object,
        crate::ir::PropertyKeyName::Name(name as crux::AtomId),
        old,
        value,
        op,
    ) {
        Ok(()) => match vm.stack.pop() {
            // `assign_member` pushed the result (the assignment's value).
            Some(result) => result.bits(),
            None => value.bits(),
        },
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn set_member_slot(ctx: *mut c_void, object: u64, name: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let object = Value::from_bits(object);
    let value = Value::from_bits(value);
    let name = name as crux::AtomId;
    let key = crux::property::PropertyKey::String(name);
    // The compiled `AssignMemberName` fast path validated the member value
    // cell (object id + name + generation), so the property is an own data
    // property; the writable check inside `write_data_property` is the
    // authoritative one (a read-warmed cell never checked it). The in-place
    // write does not bump the generation — the cell is refreshed here so
    // the compiled read probe stays warm. Any doubt (a non-writable
    // property, a shape change, an exotic receiver) falls back to the full
    // [[Set]] — which mirrors the cell on the paths that bump the
    // generation, keeping the fast path warm next time.
    if let Some(obj) = object.as_object()
        && obj.write_data_property(&key, value)
    {
        agent.member_value_cells[(obj.id() as usize ^ name as usize) & (MEMBER_CELLS - 1)] =
            MemberValueCell {
                id: obj.id(),
                name,
                generation: obj.generation(),
                value,
            };
        return value.bits();
    }
    let reference = member_reference(&object, &crate::ir::PropertyKeyName::Name(name), vm.strict);
    match crate::context::put_value(agent, &reference, value) {
        Ok(()) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn assign_member_computed(
    ctx: *mut c_void,
    op: u64,
    object: u64,
    key: u64,
    old: u64,
    value: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let object = Value::from_bits(object);
    let key = Value::from_bits(key);
    let value = Value::from_bits(value);
    let op = ASSIGN_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(AssignOp::Assign);
    if crate::ir::is_compound_assign(&op) {
        if crate::ir::is_nullish(&object) {
            return slow_error(
                ctx,
                crate::ir::nullish_error("Cannot set properties of null"),
            );
        }
        let key = match crate::context::to_property_key(agent, &key) {
            Ok(key) => key,
            Err(error) => return slow_error(ctx, error),
        };
        let old = Value::from_bits(old);
        match vm.assign_member(
            agent,
            object,
            crate::ir::PropertyKeyName::Key(key),
            Some(old),
            value,
            op,
        ) {
            Ok(()) => match vm.stack.pop() {
                Some(result) => result.bits(),
                None => value.bits(),
            },
            Err(error) => slow_error(ctx, error),
        }
    } else {
        match vm.assign_computed_plain(agent, object, key, value) {
            Ok(()) => match vm.stack.pop() {
                Some(result) => result.bits(),
                None => value.bits(),
            },
            Err(error) => slow_error(ctx, error),
        }
    }
}

extern "C" fn load_context(ctx: *mut c_void, depth: u64, index: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    match vm.context_chain_env(depth as usize) {
        Ok(env) => match crate::ir::context_env(&env).slot_value(index as usize) {
            Some(value) => value.bits(),
            None => slow_error(
                ctx,
                JsError::new(
                    ErrorKind::ReferenceError,
                    "Cannot access a binding before initialization".into(),
                ),
            ),
        },
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn store_context(ctx: *mut c_void, depth: u64, index: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let value = Value::from_bits(value);
    let env = match vm.context_chain_env(depth as usize) {
        Ok(env) => env,
        Err(error) => return slow_error(ctx, error),
    };
    let declarative = crate::ir::context_env(&env);
    if declarative.slot_value(index as usize).is_none() {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::ReferenceError,
                "Cannot access a binding before initialization".into(),
            ),
        );
    }
    if !declarative.slot_mutable(index as usize) {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::TypeError,
                "Assignment to constant variable".into(),
            ),
        );
    }
    declarative.set_slot(index as usize, value);
    value.bits()
}

/// Read a step out of the running compiled body by index: the closure/
/// RegExp helpers receive the step index as an immediate (the payload — the
/// function AST, enclosing chains, pattern/flags strings — is not marshalled
/// across the FFI boundary; it is read back from the body's step stream).
fn step_at(ctx: &JitCallContext, step: u64) -> Option<&crate::ir::Step> {
    // SAFETY: `ctx.body` points at the `Rc<CompiledBody>` the runtime holds
    // for the duration of the compiled call (see `JitCallContext::body`).
    let body = unsafe { &*ctx.body };
    body.steps.get(step as usize)
}

extern "C" fn create_function(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let Some(crate::ir::Step::CreateFunction {
        function,
        strict,
        outer_chain,
        per_iteration_chain,
    }) = step_at(ctx, step)
    else {
        // The compiled code passes its own step index; a mismatch is an
        // internal invariant violation.
        unreachable!("create_function on a non-CreateFunction step");
    };
    let env = agent.running_context().ok().map(|c| c.lexical_environment);
    let Some(env) = env else {
        return slow_error(
            ctx,
            JsError::new(ErrorKind::TypeError, "no running context".into()),
        );
    };
    match crate::function::instantiate_function_expression(
        agent,
        function,
        env,
        *strict,
        outer_chain.clone(),
        per_iteration_chain.clone(),
    ) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn create_arrow(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let Some(crate::ir::Step::CreateArrow {
        is_async,
        params,
        body,
        strict,
        outer_chain,
        per_iteration_chain,
    }) = step_at(ctx, step)
    else {
        unreachable!("create_arrow on a non-CreateArrow step");
    };
    let env = agent.running_context().ok().map(|c| c.lexical_environment);
    let Some(env) = env else {
        return slow_error(
            ctx,
            JsError::new(ErrorKind::TypeError, "no running context".into()),
        );
    };
    match crate::function::instantiate_arrow(
        agent,
        *is_async,
        params.clone(),
        body,
        env,
        *strict,
        outer_chain.clone(),
        per_iteration_chain.clone(),
    ) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn create_function_decl(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let Some(crate::ir::Step::FunctionDeclInit {
        function,
        frame_slot,
        context_slot,
        outer_chain,
        per_iteration_chain,
        ..
    }) = step_at(ctx, step)
    else {
        unreachable!("create_function_decl on a non-FunctionDeclInit step");
    };
    let env = agent.running_context().ok().map(|c| c.lexical_environment);
    let Some(env) = env else {
        return slow_error(
            ctx,
            JsError::new(ErrorKind::TypeError, "no running context".into()),
        );
    };
    let value = match crate::function::instantiate_function(
        agent,
        function,
        env,
        vm.strict,
        outer_chain.clone(),
        per_iteration_chain.clone(),
        false,
    ) {
        Ok(value) => value,
        Err(error) => return slow_error(ctx, error),
    };
    // The declaration's binding is either a frame slot or a capture-context
    // slot — mirrors the interpreter's `Step::FunctionDeclInit` arm.
    if let Some(slot) = frame_slot {
        *vm.frame_get_mut(*slot) = value;
    } else if let Some(index) = context_slot {
        let env = match vm.context_chain_env(0) {
            Ok(env) => env,
            Err(error) => return slow_error(ctx, error),
        };
        crate::ir::context_env(&env).set_slot(*index, value);
    } else {
        unreachable!("FunctionDeclInit without a binding slot (the scan allocated one)");
    }
    value.bits()
}

extern "C" fn new_target(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    match crate::context::get_new_target(agent) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn regexp_literal(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let Some(crate::ir::Step::RegExpLiteral { pattern, flags }) = step_at(ctx, step) else {
        unreachable!("regexp_literal on a non-RegExpLiteral step");
    };
    match crate::expr::eval_regexp_literal(agent, pattern, flags) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn tail_call(
    ctx: *mut c_void,
    callee: u64,
    this: u64,
    argc: u64,
    args: *mut u64,
    direct_eval: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let callee = Value::from_bits(callee);
    let this = Value::from_bits(this);
    // The arguments live in the JIT's private buffer (helpers receive
    // `&mut Vm` and may reallocate `vm.stack`, so the machine code never
    // aliases it); copy them out like `tail_call_shared` does from the
    // interpreter stack.
    let args: Vec<Value> =
        unsafe { std::slice::from_raw_parts(args as *const Value, argc as usize) }.to_vec();
    // Direct eval in tail position (`return eval(x)`), mirroring
    // `tail_call_shared`'s eval arm: the eval'd script runs with the
    // caller's environment, so the frame replacement never applies.
    if direct_eval != 0
        && match crate::ir::is_eval_function(agent, &callee) {
            Ok(eval) => eval,
            Err(error) => return slow_error(ctx, error),
        }
    {
        let source = args.first().cloned().unwrap_or(Value::Undefined);
        if !matches!(source.kind(), ValueKind::String(_)) {
            return source.bits();
        }
        let source = match crux::convert::to_string(&source) {
            Ok(source) => source,
            Err(error) => return slow_error(ctx, error),
        };
        return match crate::script::perform_eval(agent, &source, vm.strict, true) {
            Ok(result) => result.bits(),
            Err(error) => slow_error(ctx, error),
        };
    }
    // Frame replacement for an ordinary-callable ECMAScript callee in a
    // single realm; everything else takes the normal call path whose result
    // completes this body's return (mirrors `tail_call_shared`).
    let replaced = (|| -> Result<Option<std::rc::Rc<crate::ir::CompiledBody>>, JsError> {
        if let ValueKind::Function(function) = callee.kind()
            && matches!(function.kind, crux::function::FunctionKind::EcmaScript)
            && agent.realm_count.get() == 1
        {
            return vm.tail_prepare_ordinary(agent, &function, this, &args);
        }
        Ok(None)
    })();
    match replaced {
        Ok(Some(ir)) => {
            // The frame is replaced and the Vm is reset for `ir`; the
            // runtime loops on it (TCO semantics: bounded native stack).
            ctx.tail = true;
            vm.tail_replaced = Some(ir);
            0
        }
        Ok(None) => match crate::function::call_inner(agent, &callee, this, &args) {
            Ok(value) => value.bits(),
            Err(error) => slow_error(ctx, error),
        },
        Err(error) => slow_error(ctx, error),
    }
}

// ----- Cut 49: the vector call form (≥3 args or a spread) -----
//
// The compiler emits `ArgsBase`/`ArgsPush`/`ArgsSpread` to build the
// argument vector in `Vm::args` (the same channel the interpreter's
// `Step::Call`/`Step::TailCall` vector handlers read), then the vector
// `Call`/`TailCall` steps. The JIT lowers the vector-build steps to these
// helpers and bridges the work-buffer operands onto `vm.stack` for the
// existing vector handlers (mirroring `call_slow`).

extern "C" fn args_base(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    vm.args_base_stack.push(vm.args.len());
    0
}

/// Collect `iterable`'s elements (the spread protocol, spec 7.4), with the
/// for-of machinery's dense-Array fast path: a plain Array with the stock
/// `@@iterator` iterates via the generation-validated element cache instead
/// of creating the iterator object and calling `next()` per element
/// (observably identical — the stock iterator is empty and unobservable).
/// Mirrors `for_of_next`'s element read: a cache miss (a hole or structural
/// change) falls back to the full Get. The generic path is the existing
/// `get_iterator`/`iterator_step` loop, unchanged.
fn spread_elements(agent: &mut Agent, iterable: &Value) -> Result<Vec<Value>, JsError> {
    match crate::expr::for_of_begin(agent, iterable)? {
        crate::expr::ForOfState::FastArray(array) => {
            let length = Vm::array_length(agent, &array)?;
            let mut values = Vec::with_capacity(length as usize);
            for index in 0..length {
                let value = match Vm::array_element_get(agent, &array, index) {
                    Some(value) => value,
                    None => {
                        let key = crux::property::PropertyKey::from_utf8(&index.to_string());
                        crate::context::get_property_key(agent, &array, &key, array)?
                    }
                };
                values.push(value);
            }
            Ok(values)
        }
        // The generic path: the full iterator protocol. The record comes
        // from `for_of_begin` — the `@@iterator` method was fetched exactly
        // once (re-fetching would fire a getter twice, an observable
        // divergence).
        crate::expr::ForOfState::Generic(record) => {
            let mut values = Vec::new();
            loop {
                match crate::expr::iterator_step(agent, &record) {
                    Ok(Some(value)) => values.push(value),
                    Ok(None) => return Ok(values),
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

extern "C" fn args_push(ctx: *mut c_void, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    vm.args.push(Value::from_bits(value));
    0
}

extern "C" fn args_spread(ctx: *mut c_void, iterable: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let iterable = Value::from_bits(iterable);
    match spread_elements(agent, &iterable) {
        Ok(values) => {
            vm.args.extend(values);
            0
        }
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn call_vector(ctx: *mut c_void, this: u64, callee: u64, direct_eval: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let entry_len = vm.stack.len();
    let base = match vm.args_base_stack.pop() {
        Some(base) => base,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::SyntaxError,
                    "Call without an argument boundary".into(),
                ),
            );
        }
    };
    let args = vm.args.split_off(base);
    let argc = args.len();
    vm.stack.push(Value::from_bits(this));
    vm.stack.push(Value::from_bits(callee));
    vm.stack.extend(args);
    // The vector form now routes through the SAME fast-form core as a
    // `CallFast` site: `do_call_fast`'s `fast_call_core` handles the
    // certified-leaf inline run (JIT `run_jit_leaf` or the interpreter's
    // `run_inline_leaf` on this Vm — no pool round-trip, no execution-
    // context push), direct eval, the callable check, and the general call.
    match vm.do_call_fast(agent, argc, direct_eval != 0) {
        Ok(()) => {
            let result = match vm.stack.pop() {
                Some(value) => value,
                None => {
                    vm.stack.truncate(entry_len);
                    return slow_error(
                        ctx,
                        JsError::new(
                            ErrorKind::TypeError,
                            "the JIT vector call produced no result".into(),
                        ),
                    );
                }
            };
            debug_assert_eq!(vm.stack.len(), entry_len);
            result.bits()
        }
        Err(error) => {
            vm.stack.truncate(entry_len);
            slow_error(ctx, error)
        }
    }
}

extern "C" fn tail_call_vector(ctx: *mut c_void, this: u64, callee: u64, direct_eval: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let base = match vm.args_base_stack.pop() {
        Some(base) => base,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::SyntaxError,
                    "TailCall without an argument boundary".into(),
                ),
            );
        }
    };
    let args = vm.args.split_off(base);
    let this = Value::from_bits(this);
    let callee = Value::from_bits(callee);
    // GC-2: the argument copy is a local `Vec<Value>` the stack scan cannot
    // see, and the callee setup that follows allocates — suppress
    // `--gc-stress` collections for the window (mirror `tail_call_shared`).
    let _stress = crate::ir::StressSuppress::new();
    // Direct eval in tail position, mirroring `tail_call`'s eval arm: the
    // eval'd script runs with the caller's environment, so the frame
    // replacement never applies.
    if direct_eval != 0
        && match crate::ir::is_eval_function(agent, &callee) {
            Ok(eval) => eval,
            Err(error) => return slow_error(ctx, error),
        }
    {
        let source = args.first().cloned().unwrap_or(Value::Undefined);
        if !matches!(source.kind(), ValueKind::String(_)) {
            return source.bits();
        }
        let source = match crux::convert::to_string(&source) {
            Ok(source) => source,
            Err(error) => return slow_error(ctx, error),
        };
        return match crate::script::perform_eval(agent, &source, vm.strict, true) {
            Ok(result) => result.bits(),
            Err(error) => slow_error(ctx, error),
        };
    }
    // Frame replacement for an ordinary-callable ECMAScript callee in a
    // single realm; everything else takes the normal call path whose result
    // completes this body's return (mirrors `tail_call`).
    let replaced = (|| -> Result<Option<std::rc::Rc<crate::ir::CompiledBody>>, JsError> {
        if let ValueKind::Function(function) = callee.kind()
            && matches!(function.kind, crux::function::FunctionKind::EcmaScript)
            && agent.realm_count.get() == 1
        {
            return vm.tail_prepare_ordinary(agent, &function, this, &args);
        }
        Ok(None)
    })();
    match replaced {
        Ok(Some(ir)) => {
            ctx.tail = true;
            vm.tail_replaced = Some(ir);
            0
        }
        Ok(None) => match crate::function::call_inner(agent, &callee, this, &args) {
            Ok(value) => value.bits(),
            Err(error) => slow_error(ctx, error),
        },
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn tail_call_self_vector(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    // SAFETY: `ctx.body` points at the `Rc<CompiledBody>` the runtime holds
    // for the duration of the compiled call (see `JitCallContext::body`).
    let body = unsafe { &*ctx.body };
    let Some(scope) = body.scope.as_ref() else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::TypeError,
                "self-tail-call without a certified scope".into(),
            ),
        );
    };
    let base = match vm.args_base_stack.pop() {
        Some(base) => base,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::SyntaxError,
                    "TailCall without an argument boundary".into(),
                ),
            );
        }
    };
    let args = vm.args.split_off(base);
    let argc = args.len();
    // GC-2: the argument copy is a local `Vec<Value>` the stack scan cannot
    // see (mirror `tail_call_shared`'s suppression window).
    let _stress = crate::ir::StressSuppress::new();
    // Rebind the frame IN PLACE: the JIT's frame pointer stays live across
    // the jump, so a `reset`/`setup_frame` (which can reallocate the buffer)
    // is out — the machine code's re-entry block re-seeds the per-run
    // variables instead, exactly like the fast-form self jump.
    let frame = match &mut vm.frame {
        crate::ir::Frame::Inline(buf) => buf.as_mut_ptr(),
        crate::ir::Frame::Heap(vec) => vec.as_mut_ptr(),
    };
    // The parameter slots copy straight from the argument vector; the
    // remaining slots go back to their entry state (tdz-aware).
    let params = scope.arity.min(argc);
    // SAFETY: `args` holds `argc` slots and the frame holds `frame_size`;
    // the buffers are distinct allocations.
    unsafe { std::ptr::copy_nonoverlapping(args.as_ptr(), frame, params) };
    for slot in params..scope.frame_size {
        let value = if scope.tdz_store.get(slot).copied().unwrap_or(false) {
            Value::uninitialized()
        } else {
            Value::Undefined
        };
        // SAFETY: the frame buffer holds `frame_size` slots.
        unsafe { *frame.add(slot) = value };
    }
    let _ = agent;
    1
}

extern "C" fn array_begin(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match crate::builtins::array::array_create(agent, 0.0) {
        Ok(array) => {
            vm.array_index_stack.push(0);
            Value::Object(array).bits()
        }
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn array_element(ctx: *mut c_void, array: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let array = Value::from_bits(array);
    let value = Value::from_bits(value);
    let index = match vm.array_index_stack.last_mut() {
        Some(index) => *index,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::SyntaxError,
                    "ArrayElement without an array".into(),
                ),
            );
        }
    };
    if let Err(error) = crate::ir::array_set(&array, &index.to_string(), value) {
        return slow_error(ctx, error);
    }
    *vm.array_index_stack.last_mut().expect("an array is open") = index + 1;
    array.bits()
}

extern "C" fn array_spread(ctx: *mut c_void, array: u64, iterable: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let array = Value::from_bits(array);
    let iterable = Value::from_bits(iterable);
    let start = match vm.array_index_stack.last_mut() {
        Some(index) => *index,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::SyntaxError,
                    "ArraySpread without an array".into(),
                ),
            );
        }
    };
    let values = match spread_elements(agent, &iterable) {
        Ok(values) => values,
        Err(error) => return slow_error(ctx, error),
    };
    let mut index = start;
    for value in values {
        if let Err(error) = crate::ir::array_set(&array, &index.to_string(), value) {
            return slow_error(ctx, error);
        }
        index += 1;
    }
    *vm.array_index_stack.last_mut().expect("an array is open") = index;
    array.bits()
}

extern "C" fn array_hole(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    match vm.array_index_stack.last_mut() {
        Some(index) => {
            *index += 1;
            0
        }
        None => slow_error(
            ctx,
            JsError::new(ErrorKind::SyntaxError, "ArrayHole without an array".into()),
        ),
    }
}

extern "C" fn array_end(ctx: *mut c_void, array: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let array = Value::from_bits(array);
    let length = match vm.array_index_stack.pop() {
        Some(length) => length,
        None => {
            return slow_error(
                ctx,
                JsError::new(ErrorKind::SyntaxError, "ArrayEnd without an array".into()),
            );
        }
    };
    let ValueKind::Object(obj) = array.kind() else {
        return slow_error(
            ctx,
            JsError::new(ErrorKind::TypeError, "not an object".into()),
        );
    };
    match obj.set(
        &crux::JsString::from_utf8("length"),
        Value::Number(length as f64),
        true,
    ) {
        Ok(_) => array.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

// ----- Cut 53: object literals -----
//
// One helper per step, mirroring the interpreter's handlers: `ObjectBegin`
// creates the plain object (the realm's Object.prototype), the init/method/
// accessor steps define the properties with the object riding the work
// stack, `ObjectSpread` copies an iterable's own enumerable properties. The
// method/accessor steps carry their function payloads in the running body
// and read them back via the step index (the Cut 44 pattern).

extern "C" fn object_begin(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let proto = match agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.object_prototype())
        .and_then(|value| crate::context::as_object(&value))
    {
        Some(proto) => proto,
        None => {
            return slow_error(
                ctx,
                JsError::new(ErrorKind::TypeError, "no realm Object.prototype".into()),
            );
        }
    };
    Value::Object(crux::object::JsObject::ordinary_object_create(Some(proto))).bits()
}

extern "C" fn object_init_name(
    ctx: *mut c_void,
    object: u64,
    name: u64,
    set_name: u64,
    shorthand: u64,
    value: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let object = Value::from_bits(object);
    let value = Value::from_bits(value);
    match crate::ir::object_init(
        &object,
        &syntax::ast::PropertyName::Ident(name as crux::AtomId),
        value,
        set_name != 0,
        shorthand != 0,
    ) {
        Ok(()) => object.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn object_init_computed(
    ctx: *mut c_void,
    object: u64,
    key: u64,
    set_name: u64,
    value: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let object = Value::from_bits(object);
    let key = Value::from_bits(key);
    let value = Value::from_bits(value);
    let ValueKind::Object(obj) = object.kind() else {
        return slow_error(
            ctx,
            JsError::new(ErrorKind::TypeError, "not an object".into()),
        );
    };
    let key = match crate::context::to_property_key(agent, &key) {
        Ok(key) => key,
        Err(error) => return slow_error(ctx, error),
    };
    match crate::ir::object_init_key(&obj, key, value, set_name != 0) {
        Ok(()) => object.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn object_key_to_property_key(ctx: *mut c_void, key: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let key = Value::from_bits(key);
    match crate::context::to_property_key(agent, &key) {
        Ok(crux::property::PropertyKey::String(id)) => {
            Value::String(crux::Handle::new(crux::lookup(id))).bits()
        }
        Ok(crux::property::PropertyKey::Symbol(symbol)) => Value::Symbol(symbol).bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn object_method_name(ctx: *mut c_void, object: u64, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let object = Value::from_bits(object);
    let Some(crate::ir::Step::ObjectMethodName { name, function }) = step_at(ctx, step) else {
        unreachable!("object_method_name on a non-ObjectMethodName step");
    };
    let strict = unsafe { &*ctx.body }.strict;
    match crate::ir::object_method(
        agent,
        &object,
        crux::property::PropertyKey::String(*name),
        function,
        strict,
    ) {
        Ok(()) => object.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn object_method_computed(ctx: *mut c_void, object: u64, key: u64, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let object = Value::from_bits(object);
    let key = Value::from_bits(key);
    let Some(crate::ir::Step::ObjectMethodComputed { function }) = step_at(ctx, step) else {
        unreachable!("object_method_computed on a non-ObjectMethodComputed step");
    };
    let strict = unsafe { &*ctx.body }.strict;
    let key = match crate::context::to_property_key(agent, &key) {
        Ok(key) => key,
        Err(error) => return slow_error(ctx, error),
    };
    match crate::ir::object_method(agent, &object, key, function, strict) {
        Ok(()) => object.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn object_accessor_name(ctx: *mut c_void, object: u64, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let object = Value::from_bits(object);
    let Some(crate::ir::Step::ObjectAccessorName {
        name,
        get,
        param,
        body,
    }) = step_at(ctx, step)
    else {
        unreachable!("object_accessor_name on a non-ObjectAccessorName step");
    };
    let strict = unsafe { &*ctx.body }.strict;
    match crate::ir::object_accessor(
        agent,
        &object,
        crux::property::PropertyKey::String(*name),
        *get,
        param.as_ref(),
        body,
        strict,
    ) {
        Ok(()) => object.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn object_accessor_computed(ctx: *mut c_void, object: u64, key: u64, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let object = Value::from_bits(object);
    let key = Value::from_bits(key);
    let Some(crate::ir::Step::ObjectAccessorComputed { get, param, body }) = step_at(ctx, step)
    else {
        unreachable!("object_accessor_computed on a non-ObjectAccessorComputed step");
    };
    let strict = unsafe { &*ctx.body }.strict;
    let key = match crate::context::to_property_key(agent, &key) {
        Ok(key) => key,
        Err(error) => return slow_error(ctx, error),
    };
    match crate::ir::object_accessor(agent, &object, key, *get, param.as_ref(), body, strict) {
        Ok(()) => object.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn object_spread(ctx: *mut c_void, object: u64, from: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let object = Value::from_bits(object);
    let from = Value::from_bits(from);
    let ValueKind::Object(obj) = object.kind() else {
        return slow_error(
            ctx,
            JsError::new(ErrorKind::TypeError, "not an object".into()),
        );
    };
    match crate::expr::copy_data_properties(agent, &obj, &from) {
        Ok(()) => object.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

// ----- Cut 54: string literals and template concat -----

extern "C" fn push_str(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let Some(crate::ir::Step::PushStr(text)) = step_at(ctx, step) else {
        unreachable!("push_str on a non-PushStr step");
    };
    Value::String(crux::Handle::new(text.clone())).bits()
}

extern "C" fn concat_str(ctx: *mut c_void, value: u64, acc: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let value = Value::from_bits(value);
    let acc = Value::from_bits(acc);
    let text = match crate::context::to_string(agent, &value) {
        Ok(text) => text,
        Err(error) => return slow_error(ctx, error),
    };
    let mut units = crate::ir::string_units_of(&acc);
    units.extend_from_slice(text.as_slice());
    Value::String(crux::Handle::new(crux::JsString::from_utf16(&units))).bits()
}

extern "C" fn concat_str_const(ctx: *mut c_void, acc: u64, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let acc = Value::from_bits(acc);
    let Some(crate::ir::Step::ConcatStrConst(text)) = step_at(ctx, step) else {
        unreachable!("concat_str_const on a non-ConcatStrConst step");
    };
    let mut units = crate::ir::string_units_of(&acc);
    units.extend_from_slice(text.as_slice());
    Value::String(crux::Handle::new(crux::JsString::from_utf16(&units))).bits()
}

extern "C" fn push_const(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let Some(crate::ir::Step::Push(value)) = step_at(ctx, step) else {
        unreachable!("push_const on a non-Push step");
    };
    value.bits()
}

extern "C" fn load_const(ctx: *mut c_void, step: u64, op: u64, field: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let Some(crate::ir::Step::RunRegBody { ops }) = step_at(ctx, step) else {
        unreachable!("load_const on a non-RunRegBody step");
    };
    let Some(op) = ops.get(op as usize) else {
        unreachable!("load_const op index");
    };
    let value = match (op, field) {
        (crate::ir::LeafOp::LoadConst(value), 0)
        | (crate::ir::LeafOp::BinConst { value, .. }, 0) => *value,
        (crate::ir::LeafOp::StoreMemberName { value, .. }, 1) => reg_const(value),
        (crate::ir::LeafOp::GetMemberComputed { key, .. }, 2)
        | (crate::ir::LeafOp::GetMemberComputedLocal { key, .. }, 2) => reg_const(key),
        (crate::ir::LeafOp::StoreMemberComputed { key, .. }, 3) => reg_const(key),
        (crate::ir::LeafOp::StoreMemberComputed { value, .. }, 4) => reg_const(value),
        _ => unreachable!("load_const on a const-free op/field"),
    };
    value.bits()
}

fn reg_const(operand: &crate::ir::RegOperand) -> Value {
    match operand {
        crate::ir::RegOperand::Const(value) => *value,
        _ => unreachable!("load_const field is not a Const operand"),
    }
}

extern "C" fn init_context(ctx: *mut c_void, index: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let value = Value::from_bits(value);
    match vm.context_chain_env(0) {
        Ok(env) => {
            crate::ir::context_env(&env).set_slot(index as usize, value);
            value.bits()
        }
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn update_context(
    ctx: *mut c_void,
    depth: u64,
    index: u64,
    op: u64,
    prefix: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let env = match vm.context_chain_env(depth as usize) {
        Ok(env) => env,
        Err(error) => return slow_error(ctx, error),
    };
    let declarative = crate::ir::context_env(&env);
    let old = match declarative.slot_value(index as usize) {
        Some(value) => value,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::ReferenceError,
                    "Cannot access a binding before initialization".into(),
                ),
            );
        }
    };
    if !declarative.slot_mutable(index as usize) {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::TypeError,
                "Assignment to constant variable".into(),
            ),
        );
    }
    let op = UPDATE_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(UpdateOp::Increment);
    match crate::ir::update_value(agent, &op, &old) {
        Ok((old_numeric, new)) => {
            declarative.set_slot(index as usize, new);
            (if prefix != 0 { new } else { old_numeric }).bits()
        }
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn load_per_iter(ctx: *mut c_void, depth: u64, index: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    match vm.per_iteration_env(depth as usize) {
        Ok(env) => match crate::ir::context_env(&env).slot_value(index as usize) {
            Some(value) => value.bits(),
            None => slow_error(
                ctx,
                JsError::new(
                    ErrorKind::ReferenceError,
                    "Cannot access a binding before initialization".into(),
                ),
            ),
        },
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn store_per_iter(ctx: *mut c_void, depth: u64, index: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let value = Value::from_bits(value);
    match vm.per_iteration_env(depth as usize) {
        Ok(env) => {
            crate::ir::context_env(&env).set_slot(index as usize, value);
            value.bits()
        }
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn update_per_iter(
    ctx: *mut c_void,
    depth: u64,
    index: u64,
    op: u64,
    prefix: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let env = match vm.per_iteration_env(depth as usize) {
        Ok(env) => env,
        Err(error) => return slow_error(ctx, error),
    };
    let declarative = crate::ir::context_env(&env);
    let old = match declarative.slot_value(index as usize) {
        Some(value) => value,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::ReferenceError,
                    "Cannot access a binding before initialization".into(),
                ),
            );
        }
    };
    let op = UPDATE_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(UpdateOp::Increment);
    match crate::ir::update_value(agent, &op, &old) {
        Ok((old_numeric, new)) => {
            declarative.set_slot(index as usize, new);
            (if prefix != 0 { new } else { old_numeric }).bits()
        }
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn get_var_reference(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.var_ref_stack.last() {
        Some(reference) => match crate::context::get_value(agent, reference) {
            Ok(value) => value.bits(),
            Err(error) => slow_error(ctx, error),
        },
        None => slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "GetVarReference without a resolution".into(),
            ),
        ),
    }
}

extern "C" fn update_var_reference(ctx: *mut c_void, op: u64, prefix: u64, old: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let old = Value::from_bits(old);
    let op = UPDATE_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(UpdateOp::Increment);
    let (old_numeric, new) = match crate::ir::update_value(agent, &op, &old) {
        Ok(result) => result,
        Err(error) => return slow_error(ctx, error),
    };
    let reference = match vm.var_ref_stack.pop() {
        Some(reference) => reference,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::SyntaxError,
                    "UpdateVarReference without a resolution".into(),
                ),
            );
        }
    };
    match crate::context::put_value(agent, &reference, new) {
        Ok(()) => (if prefix != 0 { new } else { old_numeric }).bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn put_var_reference_op(ctx: *mut c_void, op: u64, old: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let old = Value::from_bits(old);
    let value = Value::from_bits(value);
    let op = ASSIGN_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(AssignOp::Assign);
    let reference = match vm.var_ref_stack.pop() {
        Some(reference) => reference,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::SyntaxError,
                    "PutVarReferenceOp without a resolution".into(),
                ),
            );
        }
    };
    let new = match crate::expr::apply_compound(agent, op, &old, &value) {
        Ok(new) => new,
        Err(error) => return slow_error(ctx, error),
    };
    match crate::context::put_value(agent, &reference, new) {
        Ok(()) => new.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn pop_var_reference(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    match vm.var_ref_stack.pop() {
        Some(_) => 0,
        None => slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "PopVarReference without a resolution".into(),
            ),
        ),
    }
}

/// The body's compiled-info pointer: the per-body fast cell when set (the
/// first successful lookup stores it; an eviction clears it, so a set
/// pointer is always valid), else a consult of the installed hook
/// (compiling on first use). `1` marks a known non-compilable body, so the
/// hook is not reconsulted. `in_flight` is forwarded to the cache — its
/// eviction policy must not free an entry a running frame holds. Returns
/// null when the body has no compiled code.
pub(crate) fn lookup_info(
    hook: crate::jit::JitHook,
    ir: &std::rc::Rc<CompiledBody>,
    in_flight: bool,
) -> *const JitCompiledInfo {
    let known = ir.jit_info.get();
    if known > 1 {
        known as *const JitCompiledInfo
    } else if known == 0 {
        // SAFETY: `hook.cache` is the installed cache and `ir` is alive for
        // the call.
        let ptr = unsafe {
            (hook.lookup)(
                hook.cache,
                ir as *const std::rc::Rc<CompiledBody> as *const std::os::raw::c_void,
                in_flight,
            )
        };
        if ptr.is_null() {
            ir.jit_info.set(1);
        } else {
            ir.jit_info.set(ptr as usize);
        }
        ptr as *const JitCompiledInfo
    } else {
        std::ptr::null()
    }
}

/// The general-path JIT run (the leaf path is `Vm::run_jit_leaf`): run
/// The outcome of a general-path JIT run (Cut 45): the caller
/// (`run_compiled_body`) either completes with the value, loops on the tail-
/// replaced body, or falls back to the interpreter.
pub(crate) enum JitRunOutcome {
    /// The body completed with this value.
    Value(Value),
    /// The machine code performed a tail-call frame replacement: the Vm's
    /// `tail_replaced` field holds the next body (its frame is already set
    /// up); the caller loops on it with the same Vm.
    TailReplaced,
    /// No hook installed or no compiled code — the interpreter runs the body.
    Interp,
}

/// Run `ir`'s compiled machine code for a body running on its own Vm — one
/// that may contain calls, since leaf bodies never do (`steps_are_leaf`
/// excludes every call step). The frame is `vm.frame` (the caller set it
/// up: params, `var`s, TDZ slots, this slot); the working area is a
/// private buffer, rooted for the call's duration. Returns `Interp` when
/// no hook is installed or the body has no compiled code — the caller
/// falls back to the interpreter.
pub(crate) fn run_jit_body(
    agent: &mut Agent,
    vm: &mut Vm,
    ir: &std::rc::Rc<CompiledBody>,
) -> Result<JitRunOutcome, JsError> {
    let Some(hook) = agent.jit_hook else {
        return Ok(JitRunOutcome::Interp);
    };
    // The recursion guard (see `Vm::run_jit_leaf`): beyond the cap, fall
    // back to the interpreter so the JIT's private working buffers cannot
    // exhaust the native stack.
    if agent.jit_depth >= MAX_JIT_DEPTH {
        return Ok(JitRunOutcome::Interp);
    }
    let info_ptr = lookup_info(hook, ir, agent.jit_depth > 0);
    if info_ptr.is_null() {
        return Ok(JitRunOutcome::Interp);
    }
    // SAFETY: the cache clears the per-body fast pointer on eviction, so a
    // pointer the caller just obtained (with no frame in flight to evict)
    // is into the cache's own live entry.
    let info = unsafe { &*info_ptr };
    // SAFETY: `info.entry` is a code pointer the cache owns; a fn pointer
    // is pointer-sized, so the integer cast is exact.
    let entry: JitEntry = unsafe { std::mem::transmute(info.entry) };
    // The frame lives in `vm.frame` (the caller filled it with `setup_frame`
    // plus the this slot); the working area is a private buffer — helpers
    // receive `&mut Vm` and may reallocate `vm.stack`, so the JIT's raw
    // pointers must never alias it. A small body fits a stack array (no
    // per-call heap allocation); larger bodies spill to a Vec.
    let (frame_ptr, _frame_len): (*mut Value, usize) = match &mut vm.frame {
        crate::ir::Frame::Inline(buf) => (buf.as_mut_ptr(), buf.len()),
        crate::ir::Frame::Heap(vec) => (vec.as_mut_ptr(), vec.len()),
    };
    let work_len = info.stack_usage + JIT_STACK_SLACK;
    let (mut inline_work, mut heap_work) =
        ([Value::Undefined; INLINE_JIT_BUF], Vec::<Value>::new());
    let work: &mut [Value] = if work_len <= INLINE_JIT_BUF {
        &mut inline_work[..work_len]
    } else {
        heap_work.resize(work_len, Value::Undefined);
        &mut heap_work[..]
    };
    let work_ptr = work.as_mut_ptr() as *mut c_void;
    // The live global for the compiled `LoadGlobal` fast path (resolved and
    // cached on this Vm; the machine code re-reads its id/generation in
    // place, so a mid-run mutation invalidates the value cells).
    let global = vm.global_object(agent)?;
    // The compiled `LoadIdent` probe is sound only when the body's env chain
    // is EXACTLY the global env ([[Environment]] == the global object's
    // record, no intermediate envs): any other env — a named function
    // expression's self-binding scope, a block/catch scope, a `with` object,
    // a module env — could hold a binding of the read name that shadows the
    // global property the cell records. Certified top-level declarations
    // have [[Environment]] == the global env; nested bodies fall back to
    // the `load_ident` resolve. A certified body adds no envs mid-run (no
    // `with`/`eval` in its own statements), so one walk at entry covers the
    // whole run.
    let clean_chain = {
        let current = agent.running_context()?.lexical_environment;
        matches!(&*current, EnvRecord::Global(_)) && current.outer().is_none()
    };
    let mut ctx = JitCallContext {
        pending: false,
        error: None,
        agent: agent as *mut Agent,
        vm: vm as *mut Vm,
        global_object: global.as_ptr() as *mut c_void,
        global_value_cells: agent.global_value_cells.as_ptr() as *mut c_void,
        member_value_cells: agent.member_value_cells.as_ptr() as *mut c_void,
        clean_chain,
        buf_end: (work_ptr as usize + work_len * std::mem::size_of::<Value>()) as *mut c_void,
        leaf_epoch: 0,
        leaf_call_cache: LeafCallSiteCache::empty(),
        body: std::rc::Rc::as_ptr(ir),
        tail: false,
        current_function: vm.current_function.map(|value| value.bits()).unwrap_or(0),
    };
    // Register the vm (its trace covers `vm.frame`, `vm.stack`, and any
    // nested leaf jit roots) plus the working area for the call's duration:
    // a helper can allocate and trigger a collection, and a heap value only
    // those buffers reference must survive until the JIT stores or returns
    // it.
    agent.jit_depth += 1;
    let result = crate::ir::with_jit_run(vm, ir, work, || unsafe {
        (entry)(
            frame_ptr as *mut c_void,
            work_ptr,
            (&mut ctx as *mut JitCallContext) as *mut c_void,
        )
    });
    agent.jit_depth -= 1;
    if ctx.pending {
        return Err(ctx.error.take().expect("a pending JIT error is present"));
    }
    if ctx.tail {
        return Ok(JitRunOutcome::TailReplaced);
    }
    Ok(JitRunOutcome::Value(Value::from_bits(result)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slow_path_table_is_complete() {
        // Every helper is a real function pointer (the JIT bails on a None
        // helper, so a null here would silently drop bodies to the
        // interpreter).
        assert_ne!(JIT_SLOW_PATHS.binary_slow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.concat_strings as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.relational_slow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.update_value_slow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.to_boolean_slow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.tdz_error as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.get_member_name as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.get_member_computed as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.set_member_name as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.set_member_computed as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.call_slow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.get_global as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.set_global as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.load_ident as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.resolve_var_ident as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.put_var_reference as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.update_ident as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.assign_member_name as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.assign_member_computed as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.set_member_slot as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.load_context as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.store_context as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.init_context as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.update_context as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.load_per_iter as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.store_per_iter as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.update_per_iter as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.get_var_reference as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.update_var_reference as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.put_var_reference_op as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.pop_var_reference as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.create_function as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.create_arrow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.create_function_decl as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.new_target as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.regexp_literal as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.tail_call as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.args_base as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.args_push as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.args_spread as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.call_vector as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.tail_call_vector as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.tail_call_self_vector as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.array_begin as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.array_element as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.array_spread as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.array_hole as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.array_end as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.object_begin as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.object_init_name as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.object_init_computed as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.object_key_to_property_key as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.object_method_name as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.object_method_computed as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.object_accessor_name as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.object_accessor_computed as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.object_spread as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.push_str as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.concat_str as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.concat_str_const as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.push_const as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.load_const as usize, 0);
    }

    #[test]
    fn discriminant_tables_cover_the_enums() {
        assert_eq!(BINARY_OPS.len(), 22);
        assert_eq!(BINARY_OPS[BinaryOp::Add as usize], BinaryOp::Add);
        assert_eq!(BINARY_OPS[BinaryOp::In as usize], BinaryOp::In);
        assert_eq!(
            UPDATE_OPS[UpdateOp::Increment as usize],
            UpdateOp::Increment
        );
        assert_eq!(
            UPDATE_OPS[UpdateOp::Decrement as usize],
            UpdateOp::Decrement
        );
        assert_eq!(ASSIGN_OPS.len(), 16);
        assert_eq!(ASSIGN_OPS[AssignOp::Assign as usize], AssignOp::Assign);
        assert_eq!(
            ASSIGN_OPS[AssignOp::AddAssign as usize],
            AssignOp::AddAssign
        );
        assert_eq!(
            ASSIGN_OPS[AssignOp::NullishAssign as usize],
            AssignOp::NullishAssign
        );
    }
}
