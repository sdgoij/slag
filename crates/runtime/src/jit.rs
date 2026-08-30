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
    /// Cut 55: a control-transfer dispatch that completed the body (a
    /// `return` reaching the end of the finally chain) carries the body's
    /// result value here — the dispatch helpers signal `DISPATCH_DONE` and
    /// the compiled code returns this field's bits instead of a step target.
    pub dispatch_value: u64,
    /// Cut 58: the suspension payload a compiled body's `Yield`/`Await`
    /// helper records — valid when the machine code returns
    /// `DISPATCH_SUSPEND` (`run_jit_body` converts it to a `Suspended`
    /// outcome and saves the working region). The machine code never reads
    /// it.
    pub suspension: Option<crate::ir::Suspension>,
    /// Cut 58: the machine code's working-stack pointer at the suspension
    /// (the depth of the region `run_jit_body` saves into `Vm::jit_work`).
    pub suspend_sp: u64,
    /// Cut 58: the resume mode for a re-entered compiled body — 0 = normal
    /// (the entry jumps to the continuation block with the resume value
    /// pushed at the top of the restored region), 1 = throw, 2 = return
    /// (the entry routes through the control machinery with `resume_value`).
    pub resume_kind: u8,
    /// Cut 58: the step index to resume at (0 = a fresh run — the entry
    /// uses the stack parameter's working base).
    pub resume_ip: usize,
    /// Cut 58: the working-stack pointer the entry should use on a resume
    /// (the restored region base plus the pushed-resume-value offset).
    pub resume_sp: u64,
    /// Cut 58: the resume value for the machinery kinds (throw/return).
    pub resume_value: u64,
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
    /// `Step::EnterBlock` (Cut 55): push a declarative block environment and
    /// instantiate its declarations; `decls` are read back from the running
    /// body at `step`.
    pub enter_block: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// `Step::LeaveBlock` (Cut 55): pop the block environment.
    pub leave_block: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::EnterTry` (Cut 55): push a `TryFrame` for `handler`.
    pub enter_try: extern "C" fn(ctx: *mut c_void, handler: u64) -> u64,
    /// `Step::Exit` (Cut 55): run `control_transfer` with `Ctl::Normal`;
    /// returns the target step index to jump to.
    pub exit_try: extern "C" fn(ctx: *mut c_void, ip: u64, after: u64) -> u64,
    /// `Step::Return` in a try body (Cut 55): run `control_transfer` with
    /// `Ctl::Return`; a finally interception returns its step, a completed
    /// body signals `DISPATCH_DONE` with the value in `dispatch_value`.
    pub return_control: extern "C" fn(ctx: *mut c_void, ip: u64, value: u64) -> u64,
    /// `Step::Break` (Cut 55): `control_transfer` with `Ctl::Break`; returns
    /// the target step.
    pub break_control: extern "C" fn(ctx: *mut c_void, ip: u64, target: u64) -> u64,
    /// `Step::Continue` (Cut 55): `control_transfer` with `Ctl::Continue`;
    /// returns the target step.
    pub continue_control: extern "C" fn(ctx: *mut c_void, ip: u64, target: u64) -> u64,
    /// `Step::Throw` (Cut 55): run `throw_machinery`; a catch/finally
    /// interception returns its step, an escaping throw sets the pending
    /// error (with the thrown value attached) and signals `DISPATCH_PROPAGATE`.
    pub throw_control: extern "C" fn(ctx: *mut c_void, ip: u64, value: u64) -> u64,
    /// `Step::FinallyEnd` (Cut 55): pop the pending control and re-apply it
    /// (routing through any further finally/catch); returns the target step,
    /// `DISPATCH_DONE` (a completed return), or `DISPATCH_PROPAGATE` (an
    /// escaping throw).
    pub finally_end: extern "C" fn(ctx: *mut c_void, ip: u64) -> u64,
    /// `Step::CatchBind` (Cut 55): bind the catch parameter and instantiate
    /// the catch body's declarations; the parameter is read back from the
    /// running body at `step`.
    pub catch_bind: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// The pending-error dispatch (Cut 55): route the context's pending
    /// `JsError` through `throw_machinery` as a thrown value; returns the
    /// catch/finally step, or `DISPATCH_PROPAGATE` when the throw escapes
    /// the body (the pending error is re-set with the value attached).
    pub dispatch_error: extern "C" fn(ctx: *mut c_void, ip: u64) -> u64,
    /// `Step::SwitchDisc` (Cut 56): store the popped discriminant.
    pub switch_disc: extern "C" fn(ctx: *mut c_void, value: u64) -> u64,
    /// `Step::SwitchTest` (Cut 56): strictly-equal the case test against the
    /// stored discriminant; returns 1 on a match (the machine code jumps to
    /// the case block), 0 otherwise.
    pub switch_test: extern "C" fn(ctx: *mut c_void, case: u64, test: u64) -> u64,
    /// `Step::ForInBegin` (Cut 57): push a for-in enumeration state for the
    /// RHS (a nullish RHS pushes an empty-key state so the loop is skipped).
    pub for_in_begin: extern "C" fn(ctx: *mut c_void, value: u64) -> u64,
    /// `Step::ForInNext` (Cut 57): advance the innermost for-in enumeration;
    /// on a live key, write it at `stack[0]` and return 1; on exhaustion,
    /// pop the state and return 0.
    pub for_in_next: extern "C" fn(ctx: *mut c_void, stack: u64) -> u64,
    /// `Step::ForOfBegin` (Cut 57): get the RHS's iterator (the fast-array
    /// verdict for a plain Array with the stock `@@iterator`) and push the
    /// entry plus its `(top, end)` boundary; `step` indexes the running
    /// body's `ForOfBegin` payload (the fixup-patched boundary span).
    pub for_of_begin: extern "C" fn(ctx: *mut c_void, step: u64, value: u64) -> u64,
    /// `Step::ForOfNext` (Cut 57): advance the innermost for-of entry; on an
    /// element, write it at `stack[0]` and return 1; on exhaustion, pop the
    /// entry and boundary and return 0. A generic `next()` error propagates
    /// without closing (the `for_of_stepping` flag stays set).
    pub for_of_next: extern "C" fn(ctx: *mut c_void, stack: u64) -> u64,
    /// `Step::ForOfNextBindLocal` (Cut 57): like `for_of_next`, landing the
    /// element directly in frame slot `slot` (the fused bind).
    pub for_of_next_bind_local: extern "C" fn(ctx: *mut c_void, slot: u64) -> u64,
    /// `Step::ForOfClose` (Cut 57): pop the innermost boundary and close a
    /// generic iterator (the fast entry has nothing to close).
    pub for_of_close: extern "C" fn(ctx: *mut c_void) -> u64,
    /// Cut 57: a compiled body's engine-error escape with a live for-of
    /// entry — close all active for-of iterators with a throw completion
    /// (mirroring `run_inner`'s uncovered-error close) so the pending error
    /// surfaces with the iterators closed.
    pub for_of_close_all: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::EnterPerIteration` (Cut 57): push the first per-iteration env
    /// of a certified loop (a fresh copy of the capture context's head
    /// slots); `step` indexes the running body's `names` payload.
    pub enter_per_iteration: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// `Step::PerIteration` (Cut 57): replace the lexical env with a fresh
    /// per-iteration env copied from the previous one (the loop exit's
    /// `LeaveBlock` restores the loop env); `step` indexes the running
    /// body's `names` payload.
    pub per_iteration: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// `Step::Yield` (Cut 58): record the suspension — the value, delegate
    /// flag, working-stack pointer, and continuation step — then signal
    /// `DISPATCH_SUSPEND`.
    pub yield_suspend:
        extern "C" fn(ctx: *mut c_void, sp: u64, value: u64, delegate: u64, ip: u64) -> u64,
    /// `Step::Await` (Cut 58): like `yield_suspend`, with an `Await`
    /// suspension.
    pub await_suspend: extern "C" fn(ctx: *mut c_void, sp: u64, value: u64, ip: u64) -> u64,
    /// `Step::DestructureBegin` (Cut 59): `GetIterator` on the value and
    /// push the record (not-done) on the destructure stack.
    pub destructure_begin: extern "C" fn(ctx: *mut c_void, value: u64) -> u64,
    /// `Step::DestructureNext` (Cut 59): step the innermost destructure
    /// iterator, returning the element bits (an exhausted iterator returns
    /// `undefined` and marks itself done). A `next()` error leaves
    /// `destructure_stepping` set so the close machinery skips it.
    pub destructure_next: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::DestructureRest` (Cut 59): collect the remaining values into a
    /// fresh array and pop the iterator (no close), returning the array bits.
    pub destructure_rest: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::DestructureObjCoercible` (Cut 59): RequireObjectCoercible of an
    /// object pattern's value, pushing it on the object stack with a fresh
    /// excluded frame.
    pub destructure_obj_coercible: extern "C" fn(ctx: *mut c_void, value: u64) -> u64,
    /// `Step::DestructureObjKey` (Cut 59): the object pattern's constant key
    /// (read from the step payload); returns the property value bits.
    pub destructure_obj_key: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// `Step::DestructureObjKeyComputed` (Cut 59): convert the popped key,
    /// record it in the exclusion set, and return the property value bits.
    pub destructure_obj_key_computed: extern "C" fn(ctx: *mut c_void, key: u64) -> u64,
    /// `Step::DestructureObjKeyStore` (Cut 59): push the converted key for
    /// the later `DestructureObjKeyGet`.
    pub destructure_obj_key_store: extern "C" fn(ctx: *mut c_void, key: u64) -> u64,
    /// `Step::DestructureObjKeyGet` (Cut 59): pop the stored key, convert it,
    /// record it in the exclusion set, and return the property value bits.
    pub destructure_obj_key_get: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::DestructureObjRest` (Cut 59): CopyDataProperties into a fresh
    /// rest object (the exclusion set read from the step payload), returning
    /// the rest object bits.
    pub destructure_obj_rest: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// `Step::DestructureClose` (Cut 59): pop the innermost destructure
    /// iterator and close it when it was not exhausted.
    pub destructure_close: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::DestructureObjEnd` (Cut 59): pop the object pattern's base and
    /// its exclusion frame.
    pub destructure_obj_end: extern "C" fn(ctx: *mut c_void) -> u64,
    /// Cut 59: a compiled body's engine-error escape with a live destructure
    /// — close all active not-done destructure iterators (mirroring
    /// `run_inner`'s uncovered-error close, skipping when `destructure_stepping`)
    /// and clear the object-pattern stacks.
    pub destructure_close_all: extern "C" fn(ctx: *mut c_void) -> u64,
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
    enter_block,
    leave_block,
    enter_try,
    exit_try,
    return_control,
    break_control,
    continue_control,
    throw_control,
    finally_end,
    catch_bind,
    dispatch_error,
    switch_disc,
    switch_test,
    for_in_begin,
    for_in_next,
    for_of_begin,
    for_of_next,
    for_of_next_bind_local,
    for_of_close,
    for_of_close_all,
    enter_per_iteration,
    per_iteration,
    yield_suspend,
    await_suspend,
    destructure_begin,
    destructure_next,
    destructure_rest,
    destructure_obj_coercible,
    destructure_obj_key,
    destructure_obj_key_computed,
    destructure_obj_key_store,
    destructure_obj_key_get,
    destructure_obj_rest,
    destructure_close,
    destructure_obj_end,
    destructure_close_all,
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
    let vm = unsafe { &mut *ctx.vm };
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
    // The Vm's lexical env is the authoritative running environment during
    // a JIT run: the interpreter's per-step context-env sync (skipped by
    // the JIT) keeps the agent context current, so reading the context
    // here would capture a STALE env after a block/catch pushed one.
    let env = vm.lexical_env;
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
    let vm = unsafe { &mut *ctx.vm };
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
    let env = vm.lexical_env;
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
    let env = vm.lexical_env;
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

// ----- Cut 55: try/catch/finally and the control-transfer dispatch -----

/// The control-dispatch helpers' return encoding: a value below
/// `DISPATCH_PROPAGATE` is the step index the machine code jumps to; the
/// sentinels signal a body-completing outcome (`DISPATCH_DONE` carries the
/// result in `dispatch_value`, `DISPATCH_PROPAGATE` re-raises the pending
/// error — the compiled code returns and the runtime surfaces it).
const DISPATCH_PROPAGATE: u64 = u64::MAX;
const DISPATCH_DONE: u64 = u64::MAX - 1;
/// Cut 58: a compiled body's `Yield`/`Await` reached — the suspension
/// payload is in the ctx and the working region depth in `suspend_sp`;
/// `run_jit_body` returns the outcome and the driver resumes later.
const DISPATCH_SUSPEND: u64 = u64::MAX - 2;

/// A thrown value escaping the body becomes the same `JsError` the
/// interpreter's `body_completion_to_value` produces — the attached value
/// round-trips through the caller's `to_throwable`, so an enclosing catch
/// observes the original thrown value.
fn throw_value_error(value: Value) -> JsError {
    JsError::new(ErrorKind::TypeError, format!("Uncaught {value:?}")).with_value(value)
}

/// Interpret a `control_transfer`/`throw_machinery` result for the compiled
/// dispatch: `Continue` returns the step the machinery set `vm.ip` to; a
/// completing return/throw maps to `DISPATCH_DONE`/`DISPATCH_PROPAGATE`;
/// an internal error reports through the context.
fn dispatch_result(
    ctx: &mut JitCallContext,
    vm: &mut Vm,
    result: Result<crate::ir::CtlResult, JsError>,
) -> u64 {
    match result {
        Ok(crate::ir::CtlResult::Continue) => vm.ip as u64,
        Ok(crate::ir::CtlResult::Done(crate::ir::VmOutcome::Completed(
            crate::flow::Completion::Return(value),
        ))) => {
            ctx.dispatch_value = value.bits();
            DISPATCH_DONE
        }
        Ok(crate::ir::CtlResult::Done(crate::ir::VmOutcome::Completed(
            crate::flow::Completion::Throw(value),
        ))) => {
            ctx.pending = true;
            ctx.error = Some(throw_value_error(value));
            DISPATCH_PROPAGATE
        }
        Ok(crate::ir::CtlResult::Done(_)) => {
            unreachable!("control transfer cannot suspend")
        }
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn enter_block(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let body = unsafe { &*ctx.body };
    let Some(crate::ir::Step::EnterBlock { decls }) = step_at(ctx, step) else {
        unreachable!("enter_block on a non-EnterBlock step");
    };
    let env = crate::env::new_declarative_environment(Some(vm.lexical_env));
    if let Err(error) = crate::eval::block_declaration_instantiation(agent, decls, &env, vm.strict)
    {
        return slow_error(ctx, error);
    }
    // A certified body's closures resolve captures through the static
    // context chain (see the interpreter's `EnterBlock` arm): the block env
    // is scaffolding to them, so it is marked context-transparent.
    if body.scope.is_some()
        && let crate::env::EnvRecord::Declarative(declarative) = &*env
    {
        declarative.mark_context_transparent();
    }
    vm.lexical_env = env;
    vm.env_stack.push(env);
    Value::Undefined.bits()
}

extern "C" fn leave_block(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let Some(popped) = vm.env_stack.pop() else {
        return slow_error(
            ctx,
            JsError::new(ErrorKind::SyntaxError, "Environment stack underflow".into()),
        );
    };
    // A certified body contains no `using` declarations (those steps bail),
    // so the popped env's disposable resources are always empty.
    debug_assert!(
        popped.drain_disposable_resources().is_empty(),
        "a certified JIT body cannot contain using declarations"
    );
    vm.lexical_env = popped.outer().unwrap_or(popped);
    Value::Undefined.bits()
}

extern "C" fn enter_try(ctx: *mut c_void, handler: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    vm.try_stack.push(crate::ir::TryFrame {
        handler: handler as usize,
        saved_env: vm.lexical_env,
        env_depth: vm.env_stack.len(),
    });
    Value::Undefined.bits()
}

extern "C" fn exit_try(ctx: *mut c_void, ip: u64, after: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let body = unsafe { &*ctx.body };
    vm.ip = ip as usize;
    let result = vm.control_transfer(
        agent,
        body,
        crate::ir::Ctl::Normal {
            after: after as usize,
        },
    );
    dispatch_result(ctx, vm, result)
}

extern "C" fn return_control(ctx: *mut c_void, ip: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let body = unsafe { &*ctx.body };
    vm.ip = ip as usize;
    let result = vm.control_transfer(
        agent,
        body,
        crate::ir::Ctl::Return {
            value: Value::from_bits(value),
        },
    );
    dispatch_result(ctx, vm, result)
}

extern "C" fn break_control(ctx: *mut c_void, ip: u64, target: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let body = unsafe { &*ctx.body };
    vm.ip = ip as usize;
    let result = vm.control_transfer(
        agent,
        body,
        crate::ir::Ctl::Break {
            target: target as usize,
        },
    );
    dispatch_result(ctx, vm, result)
}

extern "C" fn continue_control(ctx: *mut c_void, ip: u64, target: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let body = unsafe { &*ctx.body };
    vm.ip = ip as usize;
    let result = vm.control_transfer(
        agent,
        body,
        crate::ir::Ctl::Continue {
            target: target as usize,
        },
    );
    dispatch_result(ctx, vm, result)
}

extern "C" fn throw_control(ctx: *mut c_void, ip: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let body = unsafe { &*ctx.body };
    vm.ip = ip as usize;
    let result = vm.throw_machinery(agent, body, Value::from_bits(value));
    dispatch_result(ctx, vm, result)
}

extern "C" fn finally_end(ctx: *mut c_void, ip: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let body = unsafe { &*ctx.body };
    vm.ip = ip as usize;
    // Mirrors the interpreter's `FinallyEnd` handler: pop the pending
    // control, restore to its recorded environment, then re-apply it
    // (a pending throw routes through `throw_machinery` so a covering catch
    // in the same body still runs).
    let Some(pending) = vm.pending.pop() else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "FinallyEnd without a pending control".into(),
            ),
        );
    };
    match pending {
        crate::ir::PendingControl::Normal { after, env, depth } => {
            vm.restore_env(env, depth);
            let result = vm.control_transfer(agent, body, crate::ir::Ctl::Normal { after });
            dispatch_result(ctx, vm, result)
        }
        crate::ir::PendingControl::Break { target, env, depth } => {
            vm.restore_env(env, depth);
            let result = vm.control_transfer(agent, body, crate::ir::Ctl::Break { target });
            dispatch_result(ctx, vm, result)
        }
        crate::ir::PendingControl::Continue { target, env, depth } => {
            vm.restore_env(env, depth);
            let result = vm.control_transfer(agent, body, crate::ir::Ctl::Continue { target });
            dispatch_result(ctx, vm, result)
        }
        crate::ir::PendingControl::Return { value, env, depth } => {
            vm.restore_env(env, depth);
            let result = vm.control_transfer(agent, body, crate::ir::Ctl::Return { value });
            dispatch_result(ctx, vm, result)
        }
        crate::ir::PendingControl::Throw { value, env, depth } => {
            vm.restore_env(env, depth);
            let result = vm.throw_machinery(agent, body, value);
            dispatch_result(ctx, vm, result)
        }
    }
}

extern "C" fn catch_bind(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let body = unsafe { &*ctx.body };
    let Some(crate::ir::Step::CatchBind { param, decls }) = step_at(ctx, step) else {
        unreachable!("catch_bind on a non-CatchBind step");
    };
    // A caught throw discarded the try block's envs: restore to the try
    // entry state (a certified body's envs hold no `using` resources, so
    // there is nothing to dispose) before binding the parameter.
    if let Some((saved_env, depth)) = vm.pending_catch_disposal.take() {
        vm.restore_env(saved_env, depth);
    }
    let thrown = vm.thrown.take().unwrap_or(Value::Undefined);
    let old_env = vm.lexical_env;
    let env = crate::env::new_declarative_environment(Some(old_env));
    // A certified body's closures resolve captures through the static
    // context chain (see the interpreter's `CatchBind` arm): its catch envs
    // are scaffolding to them, so all three are marked context-transparent.
    if body.scope.is_some()
        && let crate::env::EnvRecord::Declarative(declarative) = &*env
    {
        declarative.mark_context_transparent();
    }
    let body_env = match param {
        Some(param) => {
            let param_env = crate::env::new_declarative_environment(Some(env));
            // Annex B.3.5: a direct eval's var-vs-lexical walk skips the
            // catch parameter's environment.
            param_env.mark_catch_param_env();
            if body.scope.is_some()
                && let crate::env::EnvRecord::Declarative(declarative) = &*param_env
            {
                declarative.mark_context_transparent();
            }
            vm.env_stack.push(env);
            let mut names = Vec::new();
            crate::script::bound_names(param, &mut names);
            for name in &names {
                if let Err(error) = param_env.create_mutable_binding(name, false) {
                    return slow_error(ctx, error);
                }
            }
            // The parameter environment is the running environment while
            // the default initializers run, so a closure captures the
            // parameter (spec 15.1.7 step 7).
            if let Ok(context) = agent.running_context_mut() {
                context.lexical_environment = param_env;
            }
            if let Err(error) = crate::binding::binding_initialization(
                agent,
                param,
                thrown,
                Some(&param_env),
                vm.strict,
            ) {
                return slow_error(ctx, error);
            }
            vm.env_stack.push(param_env);
            crate::env::new_declarative_environment(Some(param_env))
        }
        None => env,
    };
    if body.scope.is_some()
        && let crate::env::EnvRecord::Declarative(declarative) = &*body_env
    {
        declarative.mark_context_transparent();
    }
    if let Err(error) =
        crate::eval::block_declaration_instantiation(agent, decls, &body_env, vm.strict)
    {
        return slow_error(ctx, error);
    }
    vm.lexical_env = body_env;
    vm.env_stack.push(body_env);
    // A certified body's catch parameter is a flat frame slot (the scope
    // scan allocates it): write the thrown value so the slot reads in the
    // catch body see it.
    if let Some(scope) = &body.scope
        && let Some(param) = param
        && let syntax::ast::BindingPattern::Ident(name) = param
        && let Some(slot) = scope.slots.get(name)
    {
        *vm.frame_get_mut(*slot) = thrown;
    }
    Value::Undefined.bits()
}

extern "C" fn dispatch_error(ctx: *mut c_void, ip: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let body = unsafe { &*ctx.body };
    // Consume the pending error (a covered error dispatches to the catch /
    // finally; an uncovered one re-sets the pending error and signals the
    // propagate sentinel so the machine code returns and the runtime
    // surfaces it). Mirrors `run_inner_impl`'s Err arm for a covered error.
    // Cut 59: a step error while a destructuring pattern is in progress
    // closes the not-done iterators first (regardless of coverage — the
    // pattern's iterator is broken mid-pattern; a `next()` error skips via
    // the flag), and a throwing `return` replaces the error (spec 7.4.11).
    if !vm.destructure_stack.is_empty()
        && !vm.destructure_stepping
        && let Err(error) = vm.close_destructures_throw(agent)
    {
        ctx.error = Some(error);
    }
    let error = ctx.error.take().expect("a pending JIT error is present");
    ctx.pending = false;
    let value = match crate::builtins::error::to_throwable(agent, &error) {
        Ok(value) => value,
        Err(_) => crate::ir::error_message_value(&error),
    };
    vm.ip = ip as usize;
    let result = vm.throw_machinery(agent, body, value);
    dispatch_result(ctx, vm, result)
}

// ----- Cut 56: switch -----

extern "C" fn switch_disc(ctx: *mut c_void, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    vm.switch_disc = Some(Value::from_bits(value));
    Value::Undefined.bits()
}

extern "C" fn switch_test(ctx: *mut c_void, case: u64, test: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let test = Value::from_bits(test);
    let Some(disc) = vm.switch_disc else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "SwitchTest without a discriminant".into(),
            ),
        );
    };
    if crux::ops::is_strictly_equal(&disc, &test) {
        vm.ip = case as usize;
        1
    } else {
        0
    }
}

// ----- Cut 57: for-in / for-of + per-iteration envs -----

extern "C" fn for_in_begin(ctx: *mut c_void, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let rhs = Value::from_bits(value);
    if crate::expr::is_nullish(&rhs) {
        // spec ForInOfHeadEvaluation step 7.a: a nullish exprValue is a
        // break completion — the loop is skipped, not an error. The
        // empty-key state makes ForInNext pop straight to the done label.
        let dummy = crux::object::JsObject::ordinary_object_create(None);
        vm.for_in_stack.push((dummy, Vec::new(), 0));
        return Value::Undefined.bits();
    }
    let obj = match crate::context::to_object(agent, &rhs) {
        Ok(obj) => obj,
        Err(error) => return slow_error(ctx, error),
    };
    let obj = match crate::context::as_object(&obj) {
        Some(obj) => obj,
        None => {
            return slow_error(
                ctx,
                JsError::new(ErrorKind::TypeError, "for-in over a non-object".into()),
            );
        }
    };
    let keys = match crate::eval::for_in_key_levels(agent, &rhs) {
        Ok(keys) => keys,
        Err(error) => return slow_error(ctx, error),
    };
    vm.for_in_stack.push((obj, keys, 0));
    Value::Undefined.bits()
}

extern "C" fn for_in_next(ctx: *mut c_void, stack: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let Some((obj, keys, index)) = vm.for_in_stack.last_mut() else {
        return slow_error(
            ctx,
            JsError::new(ErrorKind::SyntaxError, "ForInNext without a for-in".into()),
        );
    };
    while *index < keys.len() {
        let (level, key) = keys[*index];
        *index += 1;
        // A key deleted during enumeration is skipped (spec
        // EnumerateObjectProperties step 5.a.v).
        match crate::eval::key_enumerable_at_level(obj, level, &key) {
            Ok(true) => {
                // SAFETY: the machine code passes its live working-stack
                // pointer with room for one slot.
                unsafe { *(stack as *mut u64) = key.bits() };
                return 1;
            }
            Ok(false) => {}
            Err(error) => return slow_error(ctx, error),
        }
    }
    vm.for_in_stack.pop();
    0
}

extern "C" fn for_of_begin(ctx: *mut c_void, step: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let Some(crate::ir::Step::ForOfBegin { top, end }) = step_at(ctx, step) else {
        unreachable!("for_of_begin on a non-ForOfBegin step");
    };
    let rhs = Value::from_bits(value);
    let entry = match crate::expr::for_of_begin(agent, &rhs) {
        Ok(crate::expr::ForOfState::Generic(record)) => crate::ir::ForOfEntry::Generic(record),
        Ok(crate::expr::ForOfState::FastArray(array)) => {
            crate::ir::ForOfEntry::Fast { array, index: 0 }
        }
        Err(error) => return slow_error(ctx, error),
    };
    vm.for_of_stack.push(entry);
    // The fixup-patched span drives `close_for_of_upto` on an external
    // break/return/throw (mirroring the interpreter handler).
    vm.for_of_boundaries.push((*top, *end));
    Value::Undefined.bits()
}

/// The shared element write of the for-of protocol helpers: advance the
/// innermost entry and either write the element at `stack[0]` (returning 1)
/// or pop the entry and return 0 on exhaustion. A generic `next()` error
/// propagates via `slow_error` with `for_of_stepping` left set (the error
/// path then skips the iterator close).
fn for_of_fetch(ctx: &mut JitCallContext, stack: u64) -> u64 {
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.for_of_advance(agent) {
        Ok(crate::ir::ForOfAdvance::Element(value)) => {
            // SAFETY: the machine code passes its live working-stack pointer
            // with room for one slot.
            unsafe { *(stack as *mut u64) = value.bits() };
            1
        }
        Ok(crate::ir::ForOfAdvance::Done) => 0,
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn for_of_next(ctx: *mut c_void, stack: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    for_of_fetch(ctx, stack)
}

extern "C" fn for_of_next_bind_local(ctx: *mut c_void, slot: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.for_of_advance(agent) {
        Ok(crate::ir::ForOfAdvance::Element(value)) => {
            *vm.frame_get_mut(slot as usize) = value;
            1
        }
        Ok(crate::ir::ForOfAdvance::Done) => 0,
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn for_of_close(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    vm.for_of_boundaries.pop();
    if let Some(crate::ir::ForOfEntry::Generic(iterator)) = vm.for_of_stack.pop()
        && let Err(error) = crate::expr::iterator_close(agent, &iterator)
    {
        return slow_error(ctx, error);
    }
    Value::Undefined.bits()
}

extern "C" fn for_of_close_all(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    // A generic `next()` error escapes with the iterator open (spec 14.7.6.2
    // uses `?` on the next call): mirror the interpreter's Err-arm skip
    // (`!covered && !for_of_stepping` — the flag stays set on the error
    // path of `for_of_advance`).
    if !vm.for_of_stepping {
        vm.close_for_of_throw(agent);
    }
    Value::Undefined.bits()
}

// ----- Cut 58: suspension -----

/// `Step::Yield` (Cut 58): record the suspension (the value plus the
/// delegate flag), the machine code's working-stack pointer (the depth of
/// the region the resume must restore), and the continuation step, then
/// signal `DISPATCH_SUSPEND`. The helper never errors — the yield just
/// suspends.
extern "C" fn yield_suspend(ctx: *mut c_void, sp: u64, value: u64, delegate: u64, ip: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    ctx.suspension = Some(crate::ir::Suspension::Yield {
        value: Value::from_bits(value),
        delegate: delegate != 0,
    });
    ctx.suspend_sp = sp;
    vm.ip = ip as usize;
    DISPATCH_SUSPEND
}

/// `Step::Await` (Cut 58): like `yield_suspend`, with an `Await`
/// suspension (no delegate flag).
extern "C" fn await_suspend(ctx: *mut c_void, sp: u64, value: u64, ip: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    ctx.suspension = Some(crate::ir::Suspension::Await(Value::from_bits(value)));
    ctx.suspend_sp = sp;
    vm.ip = ip as usize;
    DISPATCH_SUSPEND
}

// ----- Cut 59: destructuring -----

/// `Step::DestructureBegin` (Cut 59): GetIterator on the value and push the
/// record (not-done) on the destructure stack (spec 13.15.5.2 step 3).
extern "C" fn destructure_begin(ctx: *mut c_void, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let value = Value::from_bits(value);
    match crate::expr::get_iterator(agent, &value) {
        Ok(iterator) => {
            vm.destructure_stack.push(iterator);
            vm.destructure_done.push(false);
            Value::Undefined.bits()
        }
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::DestructureNext` (Cut 59): step the innermost destructure iterator,
/// returning the element bits. An exhausted iterator returns `undefined` and
/// marks itself done — a default initializer must run (and may suspend) even
/// after exhaustion (spec 13.15.5.2 step 5.d). A `next()` error propagates
/// with `destructure_stepping` left set (the error path then skips the
/// close).
extern "C" fn destructure_next(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let Some(index) = vm.destructure_stack.len().checked_sub(1) else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "DestructureNext without a destructure".into(),
            ),
        );
    };
    let iterator = vm.destructure_stack[index].clone();
    vm.destructure_stepping = true;
    match crate::expr::iterator_step(agent, &iterator) {
        Ok(Some(value)) => {
            vm.destructure_stepping = false;
            value.bits()
        }
        Ok(None) => {
            vm.destructure_stepping = false;
            vm.destructure_done[index] = true;
            Value::Undefined.bits()
        }
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::DestructureRest` (Cut 59): collect the remaining values of the
/// innermost destructure iterator into a fresh array, pop the iterator (no
/// close), and return the array bits (spec 13.15.5.2 step 6). A `next()`
/// error during the collection leaves the iterator open (the flag stays set
/// on the error path).
extern "C" fn destructure_rest(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let Some(index) = vm.destructure_stack.len().checked_sub(1) else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "DestructureRest without a destructure".into(),
            ),
        );
    };
    let iterator = vm.destructure_stack[index].clone();
    vm.destructure_stepping = true;
    let mut collected = Vec::new();
    loop {
        match crate::expr::iterator_step(agent, &iterator) {
            Ok(Some(value)) => collected.push(value),
            Ok(None) => break,
            Err(error) => return slow_error(ctx, error),
        }
    }
    vm.destructure_stepping = false;
    vm.destructure_stack.pop();
    vm.destructure_done.pop();
    match crate::builtins::array::array_from_values(agent, &collected) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::DestructureObjCoercible` (Cut 59): RequireObjectCoercible of an
/// object pattern's value, pushing it on the object stack with a fresh
/// exclusion frame (spec 13.15.5.6 step 2).
extern "C" fn destructure_obj_coercible(ctx: *mut c_void, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let value = Value::from_bits(value);
    if matches!(value.kind(), ValueKind::Undefined | ValueKind::Null) {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::TypeError,
                "Cannot destructure null or undefined".into(),
            ),
        );
    }
    vm.destructure_obj_stack.push(value);
    vm.destructure_excluded.push(Vec::new());
    Value::Undefined.bits()
}

/// `Step::DestructureObjKey` (Cut 59): the object pattern's constant property
/// key (read from the step payload); returns the property value bits.
extern "C" fn destructure_obj_key(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let Some(crate::ir::Step::DestructureObjKey { key }) = step_at(ctx, step) else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "DestructureObjKey without a key".into(),
            ),
        );
    };
    let Some(object) = vm.destructure_obj_stack.last().cloned() else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "DestructureObjKey without an object".into(),
            ),
        );
    };
    match crate::context::get_property_key(agent, &object, key, object) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::DestructureObjKeyComputed` (Cut 59): convert the popped key, record
/// it in the exclusion set, and return the property value bits.
extern "C" fn destructure_obj_key_computed(ctx: *mut c_void, key: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let key = Value::from_bits(key);
    let Some(object) = vm.destructure_obj_stack.last().cloned() else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "DestructureObjKeyComputed without an object".into(),
            ),
        );
    };
    match crate::context::to_property_key(agent, &key) {
        Ok(key) => {
            if let Some(frame) = vm.destructure_excluded.last_mut() {
                frame.push(key.clone());
            }
            match crate::context::get_property_key(agent, &object, &key, object) {
                Ok(value) => value.bits(),
                Err(error) => slow_error(ctx, error),
            }
        }
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::DestructureObjKeyStore` (Cut 59): push the converted computed key
/// for the later `DestructureObjKeyGet` (the pattern's key evaluates before
/// the assignment target's reference; the property read runs after it — spec
/// 13.15.5.6).
extern "C" fn destructure_obj_key_store(ctx: *mut c_void, key: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    vm.destructure_assign_keys.push(Value::from_bits(key));
    Value::Undefined.bits()
}

/// `Step::DestructureObjKeyGet` (Cut 59): pop the stored computed key, convert
/// it, record it in the exclusion set, and return the property value bits.
extern "C" fn destructure_obj_key_get(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let Some(key) = vm.destructure_assign_keys.pop() else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "DestructureObjKeyGet without a stored key".into(),
            ),
        );
    };
    let Some(object) = vm.destructure_obj_stack.last().cloned() else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "DestructureObjKeyGet without an object".into(),
            ),
        );
    };
    match crate::context::to_property_key(agent, &key) {
        Ok(key) => {
            if let Some(frame) = vm.destructure_excluded.last_mut() {
                frame.push(key.clone());
            }
            match crate::context::get_property_key(agent, &object, &key, object) {
                Ok(value) => value.bits(),
                Err(error) => slow_error(ctx, error),
            }
        }
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::DestructureObjRest` (Cut 59): CopyDataProperties into a fresh rest
/// object, excluding the pattern's static keys (read from the step payload)
/// plus the runtime-computed ones (the exclusion stack), and return the rest
/// object bits (spec 13.15.5.6 step 12).
extern "C" fn destructure_obj_rest(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let Some(crate::ir::Step::DestructureObjRest { excluded }) = step_at(ctx, step) else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "DestructureObjRest without an exclusion set".into(),
            ),
        );
    };
    let Some(object) = vm.destructure_obj_stack.last().cloned() else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "DestructureObjRest without an object".into(),
            ),
        );
    };
    let mut all = excluded.clone();
    if let Some(frame) = vm.destructure_excluded.last() {
        all.extend(frame.iter().cloned());
    }
    match crate::binding::rest_object(agent) {
        Ok(rest) => {
            match crate::binding::copy_data_properties_excluding(agent, &rest, &object, &all) {
                Ok(()) => Value::Object(rest).bits(),
                Err(error) => slow_error(ctx, error),
            }
        }
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::DestructureClose` (Cut 59): pop the innermost destructure iterator
/// and close it when it was not exhausted. Pops BEFORE closing, so a throwing
/// `return` reaches the error path with an empty destructure stack and is not
/// closed a second time (spec 13.15.5.2 step 5 + 7.4.11).
extern "C" fn destructure_close(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let Some(index) = vm.destructure_stack.len().checked_sub(1) else {
        return Value::Undefined.bits();
    };
    let done = vm.destructure_done.get(index).copied().unwrap_or(false);
    let iterator = vm.destructure_stack.pop().unwrap();
    vm.destructure_done.pop();
    if !done && let Err(error) = crate::expr::iterator_close(agent, &iterator) {
        return slow_error(ctx, error);
    }
    Value::Undefined.bits()
}

/// `Step::DestructureObjEnd` (Cut 59): pop the object pattern's base and its
/// exclusion frame.
extern "C" fn destructure_obj_end(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    vm.destructure_obj_stack.pop();
    vm.destructure_excluded.pop();
    Value::Undefined.bits()
}

/// Cut 59: a compiled body's engine-error escape with a live destructure —
/// close all active not-done destructure iterators (mirroring `run_inner`'s
/// uncovered-error close, skipping when `destructure_stepping` — a `next()`
/// error) and clear the object-pattern stacks. A throwing `return` replaces
/// the pending error (spec 7.4.11). The caller is the machine code's error
/// block (the pending byte is already set — a raw call).
extern "C" fn destructure_close_all(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    if !vm.destructure_stack.is_empty()
        && !vm.destructure_stepping
        && let Err(error) = vm.close_destructures_throw(agent)
    {
        ctx.error = Some(error);
    }
    Value::Undefined.bits()
}

/// Instantiate a fresh per-iteration env whose bindings copy `names` from
/// `source`, hanging off `outer` (both env-creation steps share the copy;
/// the caller decides the outer and whether the env joins the stack). The
/// env is marked context-transparent so a certified body's closures resolve
/// captures through the static context chain past it (the
/// `EnterPerIteration`/`PerIteration` interpreter handlers do the same).
fn per_iteration_env(
    names: &[crux::JsString],
    outer: crate::env::EnvRef,
    source: crate::env::EnvRef,
) -> Result<crate::env::EnvRef, JsError> {
    let env = crate::env::new_declarative_environment(Some(outer));
    for name in names {
        let value = source.get_binding_value(name, false)?;
        env.create_mutable_binding(name, false)?;
        env.initialize_binding(name, value)?;
    }
    if let crate::env::EnvRecord::Declarative(declarative) = &*env {
        declarative.mark_context_transparent();
    }
    Ok(env)
}

extern "C" fn enter_per_iteration(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let Some(crate::ir::Step::EnterPerIteration { names }) = step_at(ctx, step) else {
        unreachable!("enter_per_iteration on a non-EnterPerIteration step");
    };
    // The first per-iteration env of a certified loop: fresh bindings
    // copied from the capture context's head slots, pushed on the env stack
    // so the loop's exit/break `LeaveBlock` pops it (later iterations
    // re-use `PerIteration`, whose copies come from the previous env).
    let source = vm.body_context.unwrap_or(vm.lexical_env);
    let env = match per_iteration_env(names, vm.lexical_env, source) {
        Ok(env) => env,
        Err(error) => return slow_error(ctx, error),
    };
    vm.lexical_env = env;
    vm.env_stack.push(env);
    Value::Undefined.bits()
}

extern "C" fn per_iteration(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let Some(crate::ir::Step::PerIteration { names }) = step_at(ctx, step) else {
        unreachable!("per_iteration on a non-PerIteration step");
    };
    // The per-iteration environment replaces the lexical environment
    // without joining the stack; the loop's exit restores the loop env
    // directly (spec 14.7.5.6 — the copies come from the previous env).
    let last = vm.lexical_env;
    let outer = match last.outer() {
        Some(outer) => outer,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::ReferenceError,
                    "No outer environment for per-iteration bindings".into(),
                ),
            );
        }
    };
    let env = match per_iteration_env(names, outer, last) {
        Ok(env) => env,
        Err(error) => return slow_error(ctx, error),
    };
    vm.lexical_env = env;
    Value::Undefined.bits()
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
    /// Cut 58: the compiled body suspended (a `yield`/`await`): the
    /// suspension payload plus the working region saved in `Vm::jit_work`
    /// (and `vm.ip` set to the continuation) — the driver saves the Vm and
    /// resumes later via `run_jit_resume`.
    Suspended(crate::ir::Suspension),
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
        dispatch_value: 0,
        suspension: None,
        suspend_sp: 0,
        resume_kind: 0,
        resume_ip: 0,
        resume_sp: 0,
        resume_value: 0,
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
    if result == DISPATCH_SUSPEND {
        // The machine code suspended: save the working region (the buffer
        // is a per-run local) into `vm.jit_work` — the driver holds the Vm
        // across the suspension and `run_jit_resume` restores the region.
        // `vm.ip` was set by the helper to the continuation step.
        let suspension = ctx
            .suspension
            .take()
            .expect("a DISPATCH_SUSPEND result carries a payload");
        let depth = (ctx.suspend_sp as usize - work_ptr as usize) / std::mem::size_of::<Value>();
        vm.jit_work.clear();
        vm.jit_work.extend_from_slice(&work[..depth]);
        return Ok(JitRunOutcome::Suspended(suspension));
    }
    Ok(JitRunOutcome::Value(Value::from_bits(result)))
}

/// Drive a certified resumable body (async function / generator) through
/// its compiled machine code from the START (Cut 58): loop on tail-call
/// frame replacements, convert the outcomes to the interpreter shape the
/// async/generator drivers match on, and fall back to `vm.start` when the
/// body has no compiled code.
pub(crate) fn run_jit_body_loop(
    agent: &mut Agent,
    vm: &mut Vm,
    ir: &mut std::rc::Rc<CompiledBody>,
) -> Result<crate::ir::VmOutcome, JsError> {
    loop {
        match run_jit_body(agent, vm, ir)? {
            JitRunOutcome::Value(value) => {
                return Ok(crate::ir::VmOutcome::Completed(
                    crate::flow::Completion::Return(value),
                ));
            }
            JitRunOutcome::TailReplaced => {
                *ir = vm
                    .tail_replaced
                    .take()
                    .expect("a tail replacement carries the next body");
            }
            JitRunOutcome::Suspended(suspension) => {
                return Ok(crate::ir::VmOutcome::Suspended(suspension));
            }
            JitRunOutcome::Interp => return vm.start(agent, ir),
        }
    }
}

/// Drive a certified resumable body's RESUME (Cut 58): restore the working
/// region, deliver the resume, and fall back to the interpreter's
/// `vm.run`/`vm.run_abrupt` (the abrupt-of-a-plain-`yield`/`await` decision)
/// when the body has no compiled code. A tail replacement hands the rest of
/// the run to `run_jit_body_loop` (the replacement body starts fresh).
pub(crate) fn run_jit_resume_loop(
    agent: &mut Agent,
    vm: &mut Vm,
    ir: &mut std::rc::Rc<CompiledBody>,
    resume: crate::ir::Resume,
) -> Result<crate::ir::VmOutcome, JsError> {
    let suspended_at_delegate = vm
        .ip
        .checked_sub(1)
        .and_then(|ip| ir.steps.get(ip))
        .is_some_and(|step| matches!(step, crate::ir::Step::Yield { delegate: true }));
    match run_jit_resume(agent, vm, ir, resume.clone())? {
        JitRunOutcome::Value(value) => Ok(crate::ir::VmOutcome::Completed(
            crate::flow::Completion::Return(value),
        )),
        JitRunOutcome::TailReplaced => {
            *ir = vm
                .tail_replaced
                .take()
                .expect("a tail replacement carries the next body");
            // The replacement body runs from its own start (the tail
            // call consumed the resume and `tail_prepare_ordinary`
            // reset the Vm).
            run_jit_body_loop(agent, vm, ir)
        }
        JitRunOutcome::Suspended(suspension) => Ok(crate::ir::VmOutcome::Suspended(suspension)),
        JitRunOutcome::Interp => match &resume {
            crate::ir::Resume::Throw(_) | crate::ir::Resume::Return(_)
                if !suspended_at_delegate =>
            {
                vm.run_abrupt(agent, ir, resume)
            }
            _ => vm.run(agent, ir, resume),
        },
    }
}

/// Resume a suspended compiled body (Cut 58): restore the working region
/// saved at the suspension, deliver the resume (a normal value pushed on
/// top; a throw/return routed through the control machinery by the machine
/// code's entry dispatch — or delivered to a `yield*` delegation's resume
/// step), and re-enter the machine code at the continuation step. Returns
/// `Interp` when the body has no compiled code — the caller falls back to
/// the interpreter's `vm.run`/`vm.run_abrupt`.
pub(crate) fn run_jit_resume(
    agent: &mut Agent,
    vm: &mut Vm,
    ir: &std::rc::Rc<CompiledBody>,
    resume: crate::ir::Resume,
) -> Result<JitRunOutcome, JsError> {
    let Some(hook) = agent.jit_hook else {
        return Ok(JitRunOutcome::Interp);
    };
    if agent.jit_depth >= MAX_JIT_DEPTH {
        return Ok(JitRunOutcome::Interp);
    }
    let info_ptr = lookup_info(hook, ir, agent.jit_depth > 0);
    if info_ptr.is_null() {
        return Ok(JitRunOutcome::Interp);
    }
    let info = unsafe { &*info_ptr };
    let entry: JitEntry = unsafe { std::mem::transmute(info.entry) };
    // The frame lives in `vm.frame` (persisted); the working region was
    // saved into `vm.jit_work` at the suspension. Restore it into a fresh
    // buffer (one extra slot for the resume value), and the machine code's
    // entry block re-enters at `vm.ip`.
    let saved = vm.jit_work.len();
    let work_len = saved + 1 + info.stack_usage + JIT_STACK_SLACK;
    let (mut inline_work, mut heap_work) =
        ([Value::Undefined; INLINE_JIT_BUF], Vec::<Value>::new());
    let work: &mut [Value] = if work_len <= INLINE_JIT_BUF {
        &mut inline_work[..work_len]
    } else {
        heap_work.resize(work_len, Value::Undefined);
        &mut heap_work[..]
    };
    work[..saved].copy_from_slice(&vm.jit_work);
    let (frame_ptr, _frame_len): (*mut Value, usize) = match &mut vm.frame {
        crate::ir::Frame::Inline(buf) => (buf.as_mut_ptr(), buf.len()),
        crate::ir::Frame::Heap(vec) => (vec.as_mut_ptr(), vec.len()),
    };
    // An abrupt resume of a `yield*` delegation is delivered to the resume
    // step (spec 15.5.5): the value pushes and `resume_abrupt` carries the
    // kind (mirroring `vm.run`). A plain `yield`/`await` routes the abrupt
    // through the control machinery (mirroring `vm.run_abrupt`).
    let suspended_at_delegate = vm
        .ip
        .checked_sub(1)
        .and_then(|ip| ir.steps.get(ip))
        .is_some_and(|step| matches!(step, crate::ir::Step::Yield { delegate: true }));
    let (kind, resume_value, push) = match resume {
        crate::ir::Resume::Normal(value) => (0u8, value, true),
        crate::ir::Resume::Throw(value) if suspended_at_delegate => {
            vm.resume_abrupt = Some(crate::ir::ResumeAbrupt::Throw(value));
            (0, value, true)
        }
        crate::ir::Resume::Return(value) if suspended_at_delegate => {
            vm.resume_abrupt = Some(crate::ir::ResumeAbrupt::Return(value));
            (0, value, true)
        }
        crate::ir::Resume::Throw(value) => (1, value, false),
        crate::ir::Resume::Return(value) => (2, value, false),
    };
    // Cut 59: an abrupt resume of a `yield`/`await` inside a destructuring
    // pattern closes its iterators (spec 13.15.5.2 step 5 + 7.4.11,
    // mirroring `run_abrupt_inner` — a throwing `return` of a `return()`
    // resume replaces the completion with the close error).
    if kind == 1 {
        vm.close_destructures_abrupt(agent, false)?;
    } else if kind == 2 {
        vm.close_destructures_abrupt(agent, true)?;
    }
    let sp_offset = if push { saved + 1 } else { saved };
    if push {
        work[saved] = resume_value;
    }
    let work_ptr = work.as_mut_ptr() as *mut c_void;
    let global = vm.global_object(agent)?;
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
        dispatch_value: 0,
        suspension: None,
        suspend_sp: 0,
        resume_kind: kind,
        resume_ip: vm.ip,
        resume_sp: (work_ptr as usize + sp_offset * std::mem::size_of::<Value>()) as u64,
        resume_value: resume_value.bits(),
    };
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
    if result == DISPATCH_SUSPEND {
        let suspension = ctx
            .suspension
            .take()
            .expect("a DISPATCH_SUSPEND result carries a payload");
        let depth = (ctx.suspend_sp as usize - work_ptr as usize) / std::mem::size_of::<Value>();
        vm.jit_work.clear();
        vm.jit_work.extend_from_slice(&work[..depth]);
        return Ok(JitRunOutcome::Suspended(suspension));
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
