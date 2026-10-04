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
use syntax::ast::{AssignOp, BinaryOp, UnaryOp, UpdateOp};

use crate::agent::Agent;
use crate::context::ReferenceBase;
use crate::env::EnvRecord;
use crate::ir::{
    Builder, CompiledBody, EnvStack, INTRINSIC_COUNT, INTRINSICS, MEMBER_CELLS, MemberValueCell,
    PropertyKeyName, ScopeInfo, Vm,
};
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
    /// The leaf's frame size (params + vars + TDZ slots).
    pub frame_size: u32,
    /// The leaf's parameter count.
    pub arity: u32,
    /// G14: the leaf's `this` slot, or [`NO_THIS_SLOT`] when it has none.
    pub this_slot: u32,
    /// G14: whether the leaf is strict (its `this` binding keeps the
    /// receiver, so no nullish→global coercion applies).
    pub strict: u32,
    /// G14: per-slot TDZ bits for slots `0..min(frame_size, 64)` — a set bit
    /// means the slot starts `uninitialized`, else `undefined`. Slots at or
    /// above 64 are described only by `fill_ok`.
    pub tdz_mask: u64,
    /// G14: 1 when `tdz_mask` covers the whole frame, so the compiled hit
    /// path can rebuild it from this record alone. 0 for a frame wider than
    /// the mask, which keeps the hit path on the full probe (its verdict is
    /// still valid — only the descriptor is truncated).
    pub fill_ok: u32,
    /// G13: 1 when the leaf reads its environment (context or per-iteration
    /// slots), so the compiled hit path cannot call it in-frame — the
    /// `body_context`/`lexical_env` swap has to span the call. The site takes
    /// the env lane (`leaf_call_env`) instead.
    pub uses_env: u32,
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
            this_slot: NO_THIS_SLOT,
            strict: 0,
            tdz_mask: 0,
            fill_ok: 0,
            uses_env: 0,
        }
    }
}

/// G14: the `this_slot` sentinel for a leaf with no `this` slot, and the
/// widest frame the record's `tdz_mask` can describe.
pub const NO_THIS_SLOT: u32 = u32::MAX;
const TDZ_MASK_SLOTS: usize = u64::BITS as usize;

/// The per-callee leaf-call record (Cut 39, G15): the compiled `CallFast`/
/// `CallFastSlot` sites reuse the probe helper's verdict instead of calling it
/// every visit. The record is keyed by the CALLEE, not the call site — the
/// verdict (the leaf's entry and its frame layout) is a pure function of the
/// callee's compiled body, so one record serves every site that calls it, and
/// a megamorphic site (many callees through one call step) holds one record
/// per callee instead of thrashing a handful of slots.
///
/// The table lives on the `Agent` (see `JitCallContext::leaf_records`) rather
/// than inline in the per-run context: a context is created once per JIT run
/// — including the hot leaf path — so an inline table makes every run pay its
/// memset. The machine code trusts a record only when ALL of: its `code_gen`
/// matches the running context's `leaf_gen` (the shared table outlives a run,
/// and a run whose start could have evicted compiled code must re-probe rather
/// than jump to a freed entry), the ctx's LIVE `leaf_epoch` still equals
/// `epoch` (no slow-path helper that can re-enter the interpreter has run
/// since the probe), and the callee's NaN-box upper bits + payload match
/// `callee_hi`/`callee_payload` — together all 64 value bits, so the identity
/// check is exact. The probe fills `leaf_inline`; a zero `entry` caches a
/// rejection (the site falls back to `call_slow`). `#[repr(C)]` with
/// all-scalar fields: the compiled code reads the fields at fixed offsets
/// (`offset_of!`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LeafCallRecord {
    /// The callee's `bits & PAYLOAD_MASK` (the box address >> 4) at probe time.
    pub callee_payload: u64,
    /// The probe's verdict (see `LeafInlineInfo`).
    pub leaf_inline: LeafInlineInfo,
    /// The leaf-eligibility epoch at probe time (see `JitCallContext::leaf_epoch`).
    pub epoch: u32,
    /// The code generation at probe time (see `JitCallContext::leaf_gen`).
    pub code_gen: u32,
    /// The callee's `bits >> 44` (the NaN-box prefix + tag) at probe time.
    /// `u32::MAX` — impossible for a real value — marks an empty record, so an
    /// unwritten slot never matches.
    pub callee_hi: u32,
}

impl LeafCallRecord {
    /// An empty record: `callee_hi` is impossible, so the first visit probes.
    pub const fn empty() -> Self {
        Self {
            callee_payload: 0,
            leaf_inline: LeafInlineInfo::empty(),
            epoch: 0,
            code_gen: 0,
            callee_hi: u32::MAX,
        }
    }
}

/// G15: the number of direct-mapped `LeafCallRecord`s in the agent's table
/// (see `JitCallContext::leaf_records`). Sized so a megamorphic call site —
/// many callees through one call step — can hold one record per callee. A
/// collision (two callees, one slot) is only a re-probe: the exact identity
/// check still gates every reuse.
pub const LEAF_CALL_RECORDS: usize = 256;

/// The bit position [`leaf_record_slot`] shifts the callee hash down to
/// [`LEAF_CALL_RECORDS`] slots — the table size in bits. The machine code
/// (`emit_leaf_record_slot`) and the runtime share this constant, so keep
/// them in lockstep.
pub const LEAF_CALL_RECORD_SHIFT: u32 = 56;

const _: () = assert!(LEAF_CALL_RECORDS == 1 << (u64::BITS - LEAF_CALL_RECORD_SHIFT));

/// G15: the direct-mapped slot for `callee` in the agent's record table — a
/// multiplicative hash of the full NaN-boxed value taking the HIGH bits. The
/// payload is the box address >> 4, so consecutive allocations differ only in
/// the LOW bits; a low-bit fold (`>> 4`, `>> 16`, `>> 28`, or `& mask` on the
/// payload) collapsed a run of closures onto a handful of slots. Multiplying
/// by the golden ratio spreads any allocation stride across the whole table.
/// The machine code computes the same fold in `emit_leaf_record_slot`, and
/// `the_emitter_and_runtime_leaf_record_slots_agree` pins the two together:
/// a divergence is SILENT (the verdict is written to one slot and read from
/// another, so the site simply never inlines).
#[inline]
pub fn leaf_record_slot(callee: u64) -> usize {
    (callee.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> LEAF_CALL_RECORD_SHIFT) as usize
}

/// M10: the compiled `Step::CallApply` fast path inlines a dense
/// `argArray`'s elements only up to this count (the fast-form cap): the
/// element copy and the in-frame leaf would overflow the caller's working
/// buffer above it, so longer arrays fall back to the `call_apply` slow
/// path. The compiler reserves the same number of slots in the body's
/// `max_stack_usage` so the copy has room.
pub const JIT_APPLY_MAX_ARGS: usize = 64;

/// Cut 65: the field offsets of the interpreter's completion register
/// (`Vm::completion` / `Vm::completion_is_empty`) — the compiled script-path
/// completion steps write the register directly (a certified script's
/// fall-off-end completion reads it, and the jit crate cannot name the
/// `pub(crate)` `Vm` to `offset_of!` its fields).
pub const VM_COMPLETION_OFFSET: usize = std::mem::offset_of!(Vm, completion);
pub const VM_COMPLETION_IS_EMPTY_OFFSET: usize = std::mem::offset_of!(Vm, completion_is_empty);

/// The string-builder field offsets the compiled 1-unit append fast
/// path reads/writes directly (the jit crate cannot name the `pub(crate)`
/// `Vm`/`Builder`). `buf`/`len`/`cap` are explicit `#[repr(C)]` cursor fields,
/// so the offsets are stable without depending on the `Vec`'s internal layout.
pub const VM_BUILDER_OFFSET: usize = std::mem::offset_of!(Vm, builder);
pub const BUILDER_OWNER_OFFSET: usize = std::mem::offset_of!(Builder, owner);
pub const BUILDER_ENABLED_OFFSET: usize = std::mem::offset_of!(Builder, enabled);
pub const BUILDER_RHS_OK_OFFSET: usize = std::mem::offset_of!(Builder, rhs_ok);
pub const BUILDER_RHS_UNIT_OFFSET: usize = std::mem::offset_of!(Builder, rhs_unit);
pub const BUILDER_RHS_BITS_OFFSET: usize = std::mem::offset_of!(Builder, rhs_bits);
pub const BUILDER_BUF_OFFSET: usize = std::mem::offset_of!(Builder, buf);
pub const BUILDER_LEN_OFFSET: usize = std::mem::offset_of!(Builder, len);
pub const BUILDER_CAP_OFFSET: usize = std::mem::offset_of!(Builder, cap);

/// The offsets of a `Vec`'s cursor: std's `Vec` is `cap + ptr + len` — the
/// fields are private, so `offset_of!` cannot name them, and repr(Rust) is
/// free to reorder them (the measured order is `cap`, `ptr`, `len`, NOT the
/// declaration order). `VEC_LEN_OFFSET` has relied on `len` being last since
/// Cut 68; `vec_cursor_offsets_match_the_compiled_paths` asserts all three so
/// a toolchain that lays them out differently fails loudly instead of letting
/// the compiled `EnterTry`/`Exit` write through a misread pointer.
const VEC_PTR_OFFSET: usize = std::mem::size_of::<usize>();
const VEC_CAP_OFFSET: usize = 0;
const VEC_LEN_OFFSET: usize = 2 * std::mem::size_of::<usize>();

/// Cut 68: the leaf-eligibility state field offsets the compiled leaf-cache
/// gate re-validates when the epoch is stale (a disturbing helper ran): the
/// `Vm::can_inline_leaf` conditions plus the realm count — all plain `len`/
/// `Cell` reads, so the machine code re-checks them at-rest and re-stamps the
/// epoch instead of re-running the full `leaf_call_probe`. The jit crate
/// cannot name the `pub(crate)` `Vm`/`Agent`/`EnvStack`, hence the constants.
pub const VM_TRY_STACK_LEN_OFFSET: usize = std::mem::offset_of!(Vm, try_stack) + VEC_LEN_OFFSET;
/// The compiled `EnterTry`/`Exit` fast paths push and pop a `TryFrame` in
/// place, so they read the try stack's cursor and write a frame through it
/// (see `emit_enter_try`/`emit_exit_try`), plus the two `Vm` fields a frame
/// records at entry and the `ip` an exit leaves behind.
pub const VM_TRY_STACK_PTR_OFFSET: usize = std::mem::offset_of!(Vm, try_stack) + VEC_PTR_OFFSET;
pub const VM_TRY_STACK_CAP_OFFSET: usize = std::mem::offset_of!(Vm, try_stack) + VEC_CAP_OFFSET;
pub const VM_LEXICAL_ENV_OFFSET: usize = std::mem::offset_of!(Vm, lexical_env);
pub const VM_IP_OFFSET: usize = std::mem::offset_of!(Vm, ip);
/// G19: the pending `switch` discriminant (a raw `Value` `u64`) and its
/// presence flag — the compiled `SwitchDisc`/`SwitchTest` read and write them
/// in machine code.
pub const VM_SWITCH_DISC_OFFSET: usize = std::mem::offset_of!(Vm, switch_disc);
pub const VM_SWITCH_DISC_SET_OFFSET: usize = std::mem::offset_of!(Vm, switch_disc_set);
pub const VM_PENDING_LEN_OFFSET: usize = std::mem::offset_of!(Vm, pending) + VEC_LEN_OFFSET;
pub const VM_FOR_OF_STACK_LEN_OFFSET: usize =
    std::mem::offset_of!(Vm, for_of_stack) + VEC_LEN_OFFSET;
pub const VM_FOR_OF_BOUNDARIES_LEN_OFFSET: usize =
    std::mem::offset_of!(Vm, for_of_boundaries) + VEC_LEN_OFFSET;
pub const VM_FOR_IN_STACK_LEN_OFFSET: usize =
    std::mem::offset_of!(Vm, for_in_stack) + VEC_LEN_OFFSET;
pub const VM_ASYNC_FOR_OF_STACK_LEN_OFFSET: usize =
    std::mem::offset_of!(Vm, async_for_of_stack) + VEC_LEN_OFFSET;
pub const VM_DESTRUCTURE_STACK_LEN_OFFSET: usize =
    std::mem::offset_of!(Vm, destructure_stack) + VEC_LEN_OFFSET;
pub const VM_ENV_STACK_LEN_OFFSET: usize =
    std::mem::offset_of!(Vm, env_stack) + std::mem::offset_of!(EnvStack, len);
pub const AGENT_REALM_COUNT_OFFSET: usize = std::mem::offset_of!(Agent, realm_count);

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
    /// G14: the running realm's global object as a NaN-boxed value, snapshot
    /// once per call alongside `global_object` — the leaf fill's
    /// `OrdinaryCallBindThis` needs its BITS for a sloppy nullish receiver,
    /// and reading them here keeps the fill off the `Agent` borrow (the probe
    /// and the compiled hit path then bind the identical global).
    pub global_bits: u64,
    /// The `Agent::global_value_cells` array base (the JIT indexes it by
    /// `name & (GLOBAL_CELLS - 1)` and reads the `#[repr(C)]` cells).
    pub global_value_cells: *mut c_void,
    /// The `Agent::member_value_cells` array base (the compiled
    /// `GetMemberName` probe indexes it by `(object_id ^ name) &
    /// (MEMBER_CELLS - 1)` and reads the `#[repr(C)]` cells).
    pub member_value_cells: *mut c_void,
    /// The `Agent::computed_read_cells` array base (the compiled computed-read
    /// probe indexes it by `computed_read_cell_index(id, key_bits)` and reads
    /// the `#[repr(C)]` cells).
    pub computed_read_cells: *mut c_void,
    /// The `Agent::member_map_cells` array base (the compiled
    /// `GetMemberName` shape probe indexes it by `(map_id ^ name) &
    /// (MEMBER_CELLS - 1)` and reads the `#[repr(C)]` cells: a map id pins
    /// the descriptor layout for every instance of the shape, so a hit needs
    /// no per-object identity or generation).
    pub member_map_cells: *mut c_void,
    /// Whether every name the body reads through the env chain
    /// ([`CompiledBody::ident_names`]) resolves at the global environment
    /// record — the compiled `LoadIdent` probe is sound only then, because a
    /// cell hit returns the GLOBAL binding's value and the cell table is
    /// shared by name across bodies (an intervening record could hold a
    /// shadowing binding of that name). Computed once per call by
    /// `Vm::global_reads_are_unshadowed` — a certified body adds no envs of its
    /// own mid-run (no `with`/`eval` in its own statements).
    pub globals_unshadowed: bool,
    /// One-past-the-end of the JIT's working buffer (in bytes): the
    /// compiled leaf-call probe checks the inline leaf's frame + working
    /// area fits above the current stack top before accepting.
    pub buf_end: *mut c_void,
    /// Cut 39: the leaf-eligibility epoch — the compiled code bumps it after
    /// every slow-path helper that can re-enter the interpreter (a getter,
    /// setter, `valueOf`/`toString`, or nested call), and a leaf-call record is
    /// trusted only while its `epoch == leaf_epoch`. A certified body's own
    /// statements never touch the Vm stacks or realm count the probe's
    /// eligibility checks, so a helper is the only way those can change
    /// mid-run.
    pub leaf_epoch: u32,
    /// Cut 39/G15: the running agent's leaf-call record table
    /// (`Agent::leaf_records`). The compiled call sites and the slow-path
    /// helpers address it through this base pointer rather than an inline
    /// array, so a context — created once per JIT run, including the hot leaf
    /// path — costs nothing to set up regardless of the table size.
    pub leaf_records: *mut LeafCallRecord,
    /// G15: the code generation this run's records carry (see `Agent::leaf_gen`).
    /// The record table is shared across runs, so a record is trusted only
    /// while its `code_gen` matches: a run whose start could have evicted compiled
    /// code (freeing the entries a previous run recorded) re-probes instead of
    /// jumping to a freed entry.
    pub leaf_gen: u32,
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
    /// G21: the running body's compiled entry and working-area size, when the
    /// body is self-call eligible (`self_inline_ok`). A compiled `CallSlow`
    /// site whose callee IS the running closure then runs this body's entry
    /// directly with a nested frame in a private buffer, instead of
    /// re-entering `do_call_fast`. `0`/`false` for a body that is not
    /// eligible (a script, a resumable body, a leaf run) or not compiled.
    pub self_entry: u64,
    pub self_stack_usage: u64,
    /// G21: whether the running body may take the compiled self-call path
    /// (`CompiledBody::self_call_eligible`). Computed once at run entry.
    pub self_inline_ok: bool,
    /// M10: the running realm's %Function.prototype.apply%/%call% intrinsic
    /// bits at ctx setup — the compiled `Step::CallApply` fast path compares
    /// the member-read result against them to recognize the intrinsic (then
    /// rebuilds the call layout in machine code instead of the `call_apply`
    /// slow-path round trip). 0 when the body has no `CallApply` site or no
    /// realm/intrinsic is current (the identity check then never matches and
    /// the site falls back to the slow path). The intrinsics are stable for
    /// the realm's life and a certified body's own statements never switch
    /// realms, so a per-run snapshot is sound.
    pub apply_builtin_bits: u64,
    pub call_builtin_bits: u64,
    /// The running realm's identity bits for each recognized Stage-B
    /// intrinsic (index = `Intrinsic as usize`; 0 when the body has no
    /// `CallIntrinsic` site or no realm/intrinsic is current). The compiled
    /// `CallIntrinsic` site compares the resolved callee against its kind's
    /// slot, and the identity check is the retirement — a reassigned
    /// `Math.<name>` no longer matches. The intrinsics are stable for the
    /// realm's life and a certified body's own statements never switch realms,
    /// so a per-run snapshot is sound.
    pub intrinsic_bits: [u64; INTRINSIC_COUNT],
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
    /// The resume value for the machinery kinds (throw/return).
    pub resume_value: u64,
    /// The compiled-loop safe-point countdown: machine code decrements it at
    /// each loop header (a block executed once per iteration) and calls
    /// `gc_safepoint` when it reaches zero, resetting it to
    /// [`JIT_GC_PROBE_INTERVAL`]. This is the compiled analogue of the
    /// interpreter's per-back-edge allocation-budget check (a TLS read per
    /// iteration would be unreachable from machine code, so the poll is a
    /// cheap ctx field instead). Seeded per run — a compiled body is
    /// single-threaded and the field never crosses a context boundary.
    pub gc_ticks: u64,
}

/// The number of loop-header visits between compiled safe-point polls (see
/// `JitCallContext::gc_ticks`). 1024 matches the interpreter's
/// `ALLOC_BUDGET`: an allocating loop (>= 1 box per iteration) crosses the
/// budget within one interval, so the collection trigger fires at the same
/// pace the interpreter's back edges would set.
pub const JIT_GC_PROBE_INTERVAL: u64 = 1024;

/// A direct-mapped global-value cell the compiled `LoadGlobal`/`StoreGlobal`
/// fast paths read and write in place: `name` plus the capturing
/// `(global_id, generation)` validate the cached `value` against the global
/// object's LIVE identity and generation, and `slot` locates the binding's
/// property-vector entry for the store side. The generation must move on
/// EVERY change to what a global name resolves to, not just own-property
/// changes: a slow-path helper that mutated the global mid-run bumps it, and
/// so does a change to the global env's declarative record (the global env
/// bumps it for those, which is what makes a declarative binding cacheable).
/// `#[repr(C)]` with all-scalar fields: the compiled code loads the fields
/// at fixed offsets (`offset_of!`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GlobalValueCell {
    /// The global binding's atom (the cell's own slot identity check).
    pub name: crux::AtomId,
    /// The global object's identity at capture time.
    pub global_id: u64,
    /// The global object's generation at capture time (see the type doc: it
    /// also moves for the global env's declarative bindings).
    pub generation: u32,
    /// The binding's property-vector slot at capture time — the compiled
    /// `StoreGlobal` fast path passes it to `set_global_slot` (the property
    /// write cannot be inlined: the vector's enum layout is runtime
    /// internal). `u32::MAX` when the slot was never resolved or when the
    /// binding is declarative (no property backs it), which disables the
    /// store fast path (a load-only cell still validates).
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

/// Bounds `Helper as usize` for the temporary helper instrument
/// (`JIT_HELPER_STATS`); `crates/jit` asserts at compile time that every
/// `Helper` variant fits, so the array can never be indexed out of bounds in a
/// release build. `runtime` cannot name the enum (`jit` depends on `runtime`,
/// not the reverse), hence the literal with headroom.
pub const HELPER_COUNT: usize = 160;

/// Temporary instrumentation: how many times each slow-path helper entry point
/// ran, indexed by `Helper as usize` (`crates/jit/src/helpers.rs` defines the
/// order). Compiled code bumps a slot with an inline load/store, so a host
/// running several agents on several threads can lose increments — it measures,
/// it does not synchronize. Off unless `JIT_HELPER_STATS` is set when the body
/// is compiled.
pub struct HelperCounts(core::cell::UnsafeCell<[u64; HELPER_COUNT]>);

// SAFETY: the array is only touched by compiled code (which does not run
// concurrently with the dumper for a one-shot measurement) and by the dumper.
unsafe impl Sync for HelperCounts {}

impl HelperCounts {
    const fn new() -> Self {
        Self(core::cell::UnsafeCell::new([0; HELPER_COUNT]))
    }

    /// The base address compiled code bakes in as its increment target.
    pub fn base(&self) -> *mut u64 {
        self.0.get() as *mut u64
    }

    /// A copy of the counters, for the dumper.
    pub fn snapshot(&self) -> [u64; HELPER_COUNT] {
        // SAFETY: a read of the instrument's own array (see the type's doc).
        unsafe { *self.0.get() }
    }
}

/// The single instrument counter array (see [`HelperCounts`]).
pub static JIT_HELPER_COUNTS: HelperCounts = HelperCounts::new();

/// Stage O diagnostic (see [`DISPATCH_DEOPT`]): when non-negative, the
/// compiler emits a deopt guard at that step in every eligible body, so the
/// spill/resume path is exercised before any transform consumes it. Set
/// from `SLAG_JIT_DEOPT_PROBE` (read once, at the first compiled run).
pub static JIT_DEOPT_PROBE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

/// How many compiled bodies have bailed to the interpreter (Stage O
/// telemetry).
pub static JIT_DEOPT_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether each deopt is traced to stderr (`SLAG_JIT_DEOPT_TRACE`).
pub(crate) static JIT_DEOPT_TRACE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Read the Stage O deopt-probe switches once. Diagnostics only: the probe
/// is off unless the environment asks for it.
fn init_deopt_probe_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Ok(value) = std::env::var("SLAG_JIT_DEOPT_PROBE")
            && let Ok(step) = value.parse::<i64>()
        {
            JIT_DEOPT_PROBE.store(step, std::sync::atomic::Ordering::Relaxed);
        }
        if std::env::var_os("SLAG_JIT_DEOPT_TRACE").is_some() {
            JIT_DEOPT_TRACE.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
}

/// The runtime's slow-path helper table (field order mirrors
/// `jit::JitHelpers`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JitSlowPaths {
    /// Full binary-operator semantics (`apply_binary`); `op` is a
    /// `BinaryOp` discriminant.
    pub binary_slow: extern "C" fn(ctx: *mut c_void, op: u64, a: u64, b: u64) -> u64,
    /// Full unary-operator semantics (`eval_unary_value`) for the coercing
    /// kinds (`+x`, `-x`, `~x`): they may call user code (`valueOf`/
    /// `toString`) and may throw (a BigInt/Number mix), so the compiled code
    /// routes them here; `op` is a `UnaryOp` discriminant.
    pub unary_slow: extern "C" fn(ctx: *mut c_void, op: u64, value: u64) -> u64,
    /// Cut 41: the string-string `Add` fast path — the compiled `Add`
    /// checked both operands' string tags, so the rope concat runs directly
    /// (skipping `apply_binary`'s dispatch and number checks). Returns the
    /// concatenated value, or 0 when either operand is not a string (a
    /// string value's bits are never 0 — the sentinel is unreachable from
    /// the compiled path's tag check).
    pub concat_strings: extern "C" fn(ctx: *mut c_void, a: u64, b: u64) -> u64,
    /// Seed the string builder for a planned append loop.
    pub builder_bind: extern "C" fn(ctx: *mut c_void, slot: u64) -> u64,
    /// Materialize the string builder back into its slot.
    pub builder_store: extern "C" fn(ctx: *mut c_void, slot: u64) -> u64,
    /// Append `right` to the builder owning `slot`; returns the new
    /// accumulator bits (the exact generic `+` on fallback).
    pub builder_append: extern "C" fn(ctx: *mut c_void, slot: u64, right: u64) -> u64,
    /// JS relational semantics for a loop test on a non-Number; returns 1
    /// when the test holds.
    pub relational_slow: extern "C" fn(ctx: *mut c_void, op: u64, a: u64, b: u64) -> u64,
    /// The general `++`/`--` machinery on a non-Number; returns the new value.
    pub update_value_slow: extern "C" fn(ctx: *mut c_void, inc: u64, value: u64) -> u64,
    /// Full JS `ToBoolean` for a heap value; returns 1 when truthy.
    pub to_boolean_slow: extern "C" fn(ctx: *mut c_void, value: u64) -> u64,
    /// Throws the TDZ ReferenceError (reported through the context).
    pub tdz_error: extern "C" fn(ctx: *mut c_void) -> u64,
    /// The compiled-loop safe point (see `JitCallContext::gc_ticks`): runs
    /// `Agent::maybe_collect` when the allocation budget is exceeded and
    /// clears the leaf-call-cache records (a collection can recycle a freed
    /// closure box's address, which a record's payload match would then
    /// mistake for the original callee). Returns 0.
    pub gc_safepoint: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Get(o, name)`; `name` is an `AtomId`.
    pub get_member_name: extern "C" fn(ctx: *mut c_void, object: u64, name: u64) -> u64,
    /// A compiled member read whose map-cell hit recorded an ordinal at or
    /// above `INLINE_FIELDS` (Slice 3): the machine validated the shape, so
    /// read the map-described vector slot live (skipping the full-Get
    /// machinery); a hole or a divergent shape falls back to the full `Get`.
    /// `slot` is the map cell's recorded vector slot.
    pub get_member_map_slot:
        extern "C" fn(ctx: *mut c_void, object: u64, name: u64, slot: u64) -> u64,
    /// `Get(o, key)` with a computed key.
    pub get_member_computed: extern "C" fn(ctx: *mut c_void, object: u64, key: u64) -> u64,
    /// `Set(o, name, v)` (plain assignment); returns the stored value.
    pub set_member_name: extern "C" fn(ctx: *mut c_void, object: u64, name: u64, value: u64) -> u64,
    /// `Set(o, key, v)` with a computed key; returns the stored value.
    pub set_member_computed:
        extern "C" fn(ctx: *mut c_void, object: u64, key: u64, value: u64) -> u64,
    /// The fused register computed-member compound (`o[k] op= v`): `op` is
    /// an `AssignOp` discriminant; the run discards the result.
    pub rmw_compound_computed:
        extern "C" fn(ctx: *mut c_void, object: u64, key: u64, value: u64, op: u64) -> u64,
    /// The fused register computed-member update (`o[k]++` / `o[k]--`): `op`
    /// is an `UpdateOp` discriminant; the run discards the result.
    pub rmw_update_computed: extern "C" fn(ctx: *mut c_void, object: u64, key: u64, op: u64) -> u64,
    /// The general `CallFast` (a body may contain calls — leaf bodies never
    /// do): `args` points at the JIT buffer's argument region (`argc`
    /// slots). Runs the interpreter's call machinery on the Vm's own stack.
    pub call_slow: extern "C" fn(
        ctx: *mut c_void,
        callee: u64,
        this: u64,
        argc: u64,
        args: *mut u64,
        direct_eval: u64,
    ) -> u64,
    /// The compiled `Step::CallApply` (perf.md "remaining apply floor"):
    /// `args` points at the JIT buffer's argument region (`argc` slots, the
    /// `thisArg` first); `kind` is 0 for `apply`, 1 for `call`. Runs
    /// `Vm::do_call_apply` — the intrinsic check, the direct call of the
    /// receiver on this Vm (leaf-inline included), or the general fallback
    /// call of the resolved function.
    pub call_apply: extern "C" fn(
        ctx: *mut c_void,
        resolved: u64,
        callee: u64,
        argc: u64,
        args: *mut u64,
        kind: u64,
    ) -> u64,
    /// M10: the compiled `CallApply` fast path's dense-`argArray` element
    /// fill (see [`apply_args_fill`]): `arg_array` is the `apply` argument
    /// array value; `dest` is the JIT buffer address the elements are
    /// written at. Returns the element count, or `u64::MAX` when the array
    /// is not a dense fast Array / is too long / has no room (the call site
    /// then falls back to `call_apply` — nothing was written).
    pub apply_args_fill: extern "C" fn(ctx: *mut c_void, arg_array: u64, dest: u64) -> u64,
    /// Cut 37: the compiled leaf-call probe — validates the callee (a
    /// certified, environment-free, this-less leaf whose body has compiled
    /// machine code) and that the inline frame + working area fit above the
    /// current stack top in the JIT buffer, fills the leaf's frame (the
    /// params/vars/TDZ slots above the argument region; the aliased case is
    /// the arguments themselves), and returns the leaf's JIT entry (0 = the
    /// call site falls back to `call_slow`). `args` points at the argument
    /// region's first slot; `argc` is the argument count; `this` is the call's
    /// UNBOUND receiver (spec's `thisArgument` — the probe applies
    /// `OrdinaryCallBindThis` when it fills the frame's `this` slot); `slot` is
    /// the record slot the compiled call site selected (`leaf_record_slot` of
    /// the callee) — the probe records the callee identity, the code
    /// generation and the live leaf-eligibility epoch there, so the compiled
    /// code can skip the probe on repeat visits.
    pub leaf_call_probe: extern "C" fn(
        ctx: *mut c_void,
        callee: u64,
        this: u64,
        args: *mut u64,
        argc: u64,
        slot: u64,
    ) -> u64,
    /// G14: the compiled leaf-call hit path's frame rebuild for a NON-aliased
    /// frame — `leaf_call_probe` minus the validation the compiled record gate
    /// already did (the generation, the callee identity and the
    /// leaf-eligibility epoch all matched). It re-derives the callee's scope,
    /// rebuilds its frame above the argument region and returns the cached
    /// entry, or 0 when the frame no longer fits (the site falls back to
    /// `call_slow`). `slot` is the compiled call site's record slot.
    pub leaf_call_fill: extern "C" fn(
        ctx: *mut c_void,
        callee: u64,
        this: u64,
        args: *mut u64,
        argc: u64,
        slot: u64,
    ) -> u64,
    /// G13: run a certified leaf that reads its environment on the caller's
    /// ctx and buffer (the frame was already built by `leaf_call_fill`),
    /// swapping `body_context`/`lexical_env` around the compiled entry.
    /// Returns the result bits, or `u64::MAX` to fall back to `call_slow`.
    /// `slot` is the compiled call site's record slot.
    pub leaf_call_env:
        extern "C" fn(ctx: *mut c_void, callee: u64, args: *mut u64, argc: u64, slot: u64) -> u64,
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
    /// `Step::TypeofIdent`: `typeof` of a name that resolves through the
    /// environment chain; `name` is an `AtomId`. Returns the `typeof` string —
    /// `"undefined"` for an unresolvable reference, per spec 13.5.3.2 step 1.
    pub typeof_ident: extern "C" fn(ctx: *mut c_void, name: u64) -> u64,
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
    /// The JIT's inline dense-array element write (`AssignMemberComputed`
    /// with a plain `=`): returns 1 when the element was stored through
    /// `array_element_write` (the object is a plain Array and the key a
    /// canonical index Number), 0 when the machine code must fall back to
    /// `assign_member_computed`. Never sets the pending byte — the checks
    /// (Array kind, canonical index, chain walk) are exactly the fast path
    /// of `assign_computed_plain`, so the slow path is correct on 0.
    pub fast_array_element_write:
        extern "C" fn(ctx: *mut c_void, object: u64, key: u64, value: u64) -> u64,
    /// The JIT-inline dense-array append (gap-close M1 C): the compiled
    /// gate has verified the receiver is a dense Array (`array_dense` set)
    /// and the key is a canonical index Number equal to `slots.length` —
    /// this helper runs the remaining stateful append (extensibility, the
    /// chain-clean verdict, the buffer push, the length write + mirror,
    /// the generation bump) through the shared `array_element_write`
    /// machinery. Returns 1 on a stored append, 0 to fall back (the slow
    /// path re-runs everything correctly — nothing observable ran on the
    /// fast attempt). Never sets the pending byte: the chain walk bails on
    /// any non-plain link, so no trap or getter can run.
    pub dense_array_append:
        extern "C" fn(ctx: *mut c_void, object: u64, index: u64, value: u64) -> u64,
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
    /// `Step::Construct` (the vector form): the callee on the JIT buffer,
    /// the arguments in the Vm's vector, and the caller's current working
    /// `sp` — run the construct machinery (the construct-inline leaf fast
    /// path or the general path) and return the constructed value. The `sp`
    /// lets an environment-free leaf body run on the caller's ctx with a
    /// frame carved from its buffer.
    pub construct: extern "C" fn(ctx: *mut c_void, callee: u64, sp: u64) -> u64,
    /// `Step::TaggedTemplate` (Cut 78): the tag + its `this` on the JIT
    /// buffer, the substitutions in the Vm's vector above an argument
    /// boundary, and the step index (the `TemplateLiteral` payload lives in
    /// the step) — mirror the interpreter handler and run the tag.
    pub tagged_template: extern "C" fn(ctx: *mut c_void, tag: u64, this: u64, step: u64) -> u64,
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
    /// Cut 72: the compiled `Step::ObjectFast` fused whole-literal create
    /// (`object_fast(ctx, step, sp)`) — the names payload is read back from
    /// the running body and the n values sit below the machine-code working
    /// `sp` in source order. Creates the object on the forked shape and
    /// adopts all fields in one call (the interpreter's fused handler).
    pub object_fast: extern "C" fn(ctx: *mut c_void, step: u64, sp: u64) -> u64,
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
    /// a member op's `RegOperand::Const`): read the value out of the running
    /// body's register op at `(step, op)` and return its bits. `field`
    /// selects the const-bearing field of the op (0 = the op's own Value,
    /// 1 = `StoreMemberName.value`, 2 = `GetMemberComputed(.Local).key`,
    /// 3/4 = `StoreMemberComputed.key/value`, 5 = the computed-store/key or
    /// fused-RMW operand, 6 = `CompoundMemberComputedLocal.key`).
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
    pub exit_try: extern "C" fn(ctx: *mut c_void, ip: u64, after: u64, handler: u64) -> u64,
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
    /// The compiled fast-array cursor's fallback advance (G17): set the
    /// innermost `Fast` entry's index to the inline cursor's (the compiled
    /// path advances its own copy), then `for_of_next_bind_local`'s advance.
    pub for_of_fast_next: extern "C" fn(ctx: *mut c_void, slot: u64, index: u64) -> u64,
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
    /// `Step::CreateArguments` (Cut 60): build the body's `arguments` object
    /// (sloppy mapped — aliasing the formals through the capture context — or
    /// strict unmapped) and store it into the frame slot; `step` indexes the
    /// running body's `slot`/`mapped` payload.
    pub create_arguments: extern "C" fn(ctx: *mut c_void, step: u64) -> u64,
    /// `Step::TypeofTop` (Cut 60): compute the `typeof` string of the value
    /// (pops it) and return it. Never errors.
    pub typeof_top: extern "C" fn(ctx: *mut c_void, value: u64) -> u64,
    /// A compiled `GetMemberName` with the `length` atom: the slots length of
    /// an IntegerIndexed receiver, or the canonical-NaN sentinel otherwise
    /// (the machine code falls back to the member-cell probe /
    /// `get_member_name`). Pure — no user code, no Vm mutation — so it never
    /// sets the pending byte.
    pub typed_array_length: extern "C" fn(ctx: *mut c_void, object: u64) -> u64,
    /// `Step::GetSuperBase` (Cut 61): the this-binding check + the base (the
    /// home object's [[Prototype]] for a certified body, the env walk
    /// otherwise).
    pub get_super_base: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::ThisValue` (Cut 61): the current run's this binding — the
    /// certified frame slot or the Function env's binding.
    pub this_value: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::GetSuperName` (Cut 61): `base.name` with the this receiver.
    pub get_super_name: extern "C" fn(ctx: *mut c_void, base: u64, name: u64) -> u64,
    /// `Step::GetSuperComputed` (Cut 61): `base[key]` with the this receiver.
    pub get_super_computed: extern "C" fn(ctx: *mut c_void, base: u64, key: u64) -> u64,
    /// `Step::GetSuperComputedKeep` (Cut 61): like `get_super_computed`, with
    /// the converted key written at `stack[0]` for the write (the machine
    /// code advances sp past it, then pushes the returned value).
    pub get_super_computed_keep:
        extern "C" fn(ctx: *mut c_void, stack: u64, base: u64, key: u64) -> u64,
    /// `Step::AssignSuperName` (Cut 61): `super.x = v` / `super.x op= v`.
    pub assign_super_name:
        extern "C" fn(ctx: *mut c_void, op: u64, base: u64, name: u64, old: u64, value: u64) -> u64,
    /// `Step::AssignSuperComputed` (Cut 61): `super[k] = v` / `super[k] op= v`.
    pub assign_super_computed:
        extern "C" fn(ctx: *mut c_void, op: u64, base: u64, key: u64, old: u64, value: u64) -> u64,
    /// `Step::UpdateSuperName` (Cut 61): `super.x++`/`--` (prefix/postfix).
    pub update_super_name: extern "C" fn(
        ctx: *mut c_void,
        op: u64,
        prefix: u64,
        base: u64,
        name: u64,
        old: u64,
    ) -> u64,
    /// `Step::UpdateSuperComputed` (Cut 61): `super[k]++`/`--`.
    pub update_super_computed:
        extern "C" fn(ctx: *mut c_void, op: u64, prefix: u64, base: u64, key: u64, old: u64) -> u64,
    /// `Step::DeleteSuper` (Cut 61): `delete super.x` — always the
    /// ReferenceError (spec 13.5.1.2 step 4.b).
    pub delete_super: extern "C" fn(ctx: *mut c_void) -> u64,
    /// `Step::ResolveSuperRefName` (Cut 61): build the super reference on the
    /// var_ref_stack (update/logical-assign paths).
    pub resolve_super_ref_name: extern "C" fn(ctx: *mut c_void, name: u64) -> u64,
    /// `Step::ResolveSuperRefComputed` (Cut 61): the computed-key form.
    pub resolve_super_ref_computed: extern "C" fn(ctx: *mut c_void, base: u64, key: u64) -> u64,
}

/// The runtime's slow-path table, installed into every `JitHook`.
pub static JIT_SLOW_PATHS: JitSlowPaths = JitSlowPaths {
    binary_slow,
    unary_slow,
    concat_strings,
    builder_bind,
    builder_store,
    builder_append,
    relational_slow,
    update_value_slow,
    to_boolean_slow,
    tdz_error,
    gc_safepoint,
    get_member_name,
    get_member_map_slot,
    get_member_computed,
    set_member_name,
    set_member_computed,
    rmw_compound_computed,
    rmw_update_computed,
    call_slow,
    call_apply,
    apply_args_fill,
    leaf_call_probe,
    leaf_call_fill,
    leaf_call_env,
    get_global,
    set_global,
    set_global_slot,
    load_ident,
    typeof_ident,
    resolve_var_ident,
    put_var_reference,
    update_ident,
    assign_member_name,
    assign_member_computed,
    fast_array_element_write,
    dense_array_append,
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
    construct,
    tagged_template,
    tail_call_vector,
    tail_call_self_vector,
    array_begin,
    array_element,
    array_spread,
    array_hole,
    array_end,
    object_begin,
    object_fast,
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
    for_of_fast_next,
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
    create_arguments,
    typeof_top,
    typed_array_length,
    get_super_base,
    this_value,
    get_super_name,
    get_super_computed,
    get_super_computed_keep,
    assign_super_name,
    assign_super_computed,
    update_super_name,
    update_super_computed,
    delete_super,
    resolve_super_ref_name,
    resolve_super_ref_computed,
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

/// Cut 69: the number of interpreted consultations a straight-line body
/// (no loop) must receive before it is promoted to compiled code. The
/// corpus's one-shot script/function bodies run once each, so they never
/// amortize a Cranelift compile; loop bodies bypass the gate entirely
/// (they run once with many internal iterations, so a pure count would
/// never promote them). See `.notes/jit-compile-threshold.md`.
pub(crate) const JIT_COMPILE_THRESHOLD: u32 = 16;

/// The largest body (in steps) the JIT will compile at all. Compile cost is
/// roughly linear in a body's step count, and a body above this cap costs more
/// to compile than a pass over the workload repays, so the interpreter serves
/// it. Measured on deno's tsc probe (`.notes/non-leaf-jit.md` §6): in a debug
/// build the 567 compiled bodies cost **52.2 s** to compile, and the 125 of them
/// above this cap cost **36.6 s** of that (18 bodies over 256 steps alone cost
/// 17.3 s, the 2,277-step scanner ~2.8 s). Refusing them, two samples each in
/// one session: ungated **151.7 s**, this cap **129.7 s** (the two samples
/// agreeing to 0.2 s), and pre-`Unary` **136.4 s** — so the cap both removes the
/// regression the `Unary` slice's extra compiles caused and lands ahead of the
/// state before it. 514 of 567 bodies stay compiled; the compact loops that
/// probe's build wins (`--jit-bench` rows, the 2.4× leaf loop) are all far under
/// it — the claim that nothing worth compiling is refused does not survive the
/// release measurement below, which finds the opcost loops sitting just over.
/// A release build compiles ~10× cheaper, so the cap is conservative there — it
/// trades a bounded compile for an unbounded one, which is the property worth
/// keeping. A body containing a self-tail-call is exempt ([`body_has_self_tail
/// _call`]), because its iterations are unbounded within one call. The proxy is
/// the step count, so a `RunRegBody` (whose op list is inside the step) can hide
/// cost; the cap errs toward compiling it.
///
/// The calibration above is a **debug** measurement, and debug is not what
/// ships: the numbers it refuses are exactly the ones a release build compiles
/// cheaply. So the cap is per-profile — debug keeps the measured 128, release
/// gets the larger bound. Measured on the opcost family (per-file alternating
/// A/B against the 128 binary, 3 samples each, release): the seven rows whose
/// `bench()` body is 129-133 steps are refused at 128 and compile at 1024, where
/// they run 1.2×-2.0× faster — `array_push` 2.0×, `math_call` 2.0×, `set_has`
/// 1.9×, `obj_prop` 1.7×, `string_charat` 1.6×, `regexp_test` 1.2×,
/// `string_indexof` 1.0× — and no already-compiled row regressed. The
/// multi-thousand-step bodies the probe above names (the 2,277-step scanner)
/// stay refused at both. Note the step count is a body-wide proxy: a large
/// prologue around a small hot loop counts against it, which is the shape every
/// opcost row has.
#[cfg(debug_assertions)]
pub(crate) const JIT_MAX_COMPILE_STEPS: usize = 128;

/// Release's cap: see [`JIT_MAX_COMPILE_STEPS`].
#[cfg(not(debug_assertions))]
pub(crate) const JIT_MAX_COMPILE_STEPS: usize = 1024;

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

/// The `UnaryOp` variants in declaration order (a fieldless enum's
/// discriminant is its index — guaranteed by the language). Only the
/// coercing kinds and `Not` are ever emitted as a `Step::Unary`
/// (`delete`/`void`/`typeof` lower to their own steps), but the table covers
/// every variant so the helper mirrors the interpreter's error for a stray
/// one.
const UNARY_OPS: [UnaryOp; 7] = [
    UnaryOp::Delete,
    UnaryOp::Void,
    UnaryOp::Typeof,
    UnaryOp::Plus,
    UnaryOp::Minus,
    UnaryOp::BitNot,
    UnaryOp::Not,
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

/// The coercing unary operators' full semantics (`eval_unary_value`):
/// `+x`/`-x`/`~x` may call user code and may throw, so the compiled code
/// routes them here. `!x` lowers inline (a truthiness test plus a select) and
/// never reaches this helper; `delete`/`void`/`typeof` lower to their own
/// steps, and the table fallback mirrors the interpreter's error for them.
extern "C" fn unary_slow(ctx: *mut c_void, op: u64, value: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let op = UNARY_OPS.get(op as usize).copied().unwrap_or(UnaryOp::Not);
    match crate::expr::eval_unary_value(agent, &op, Value::from_bits(value)) {
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

/// Seed the per-Vm string builder from `slot`.
extern "C" fn builder_bind(ctx: *mut c_void, slot: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    if ctx.vm.is_null() {
        return 0;
    }
    let vm = unsafe { &mut *ctx.vm };
    vm.builder_bind(slot as usize);
    0
}

/// Materialize the per-Vm string builder back into `slot`.
extern "C" fn builder_store(ctx: *mut c_void, slot: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    if ctx.vm.is_null() {
        return 0;
    }
    let vm = unsafe { &mut *ctx.vm };
    vm.builder_store(slot as usize);
    0
}

/// Append `right` to the builder owning `slot`, or run the exact
/// generic `slot = slot + right`. Returns the accumulator bits.
extern "C" fn builder_append(ctx: *mut c_void, slot: u64, right: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    if ctx.vm.is_null() {
        return right;
    }
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let right = Value::from_bits(right);
    if vm.builder_try_append(slot as usize, &right) {
        return right.bits();
    }
    match vm.builder_generic_add(agent, slot as usize, &right) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
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

/// The compiled-loop safe point: the machine code polls every
/// [`JIT_GC_PROBE_INTERVAL`] iterations (the interpreter checks the same
/// budget at every loop back edge); when enough boxes were allocated since
/// the last check, run the real collection trigger. Can never set the
/// pending error — a GC runs no user code (finalizer callbacks are queued
/// as jobs). The leaf-call-cache records are cleared when a sweep actually
/// freed a box: a record matches its callee by payload, so a freed box whose
/// address a later allocation recycles could match it and apply the cached
/// verdict to the new object. A crossing that freed nothing — with the nursery
/// pacing minors, most of them — keeps the verdicts, so an allocating compiled
/// loop no longer re-probes its call sites on every budget crossing.
extern "C" fn gc_safepoint(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    // The termination request is read on every probe and before the budget: a
    // compiled loop has no other check point, so the read cannot be conditional
    // on a collection being due.
    if unsafe { &*ctx.agent }.is_terminating() {
        return slow_error(ctx, crate::agent::termination_error());
    }
    if !crux::heap::allocation_budget_exceeded() {
        return 0;
    }
    let agent = unsafe { &mut *ctx.agent };
    agent.maybe_collect();
    if crux::heap::take_swept_since_check() > 0 {
        agent.leaf_records.fill(LeafCallRecord::empty());
    }
    0
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

extern "C" fn get_member_map_slot(ctx: *mut c_void, object: u64, name: u64, slot: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let object = Value::from_bits(object);
    let name = name as crux::AtomId;
    // The compiled shape probe validated the receiver's CURRENT map describes
    // `name` at `slot` (an ordinal >= INLINE_FIELDS; a map id pins the
    // descriptor layout, and the transition discipline puts the described
    // key's vector entry at the ordinal on every map-carrying object), so
    // read the map-described vector slot live — no full-Get re-derivation.
    // Warm the value cell like the map path's hit (the next read — and the
    // compiled probe — serves from it). A hole (a boilerplate pre-sized
    // field the body skipped — not an own property) or a divergent shape
    // falls back to the full Get (the prototype chain must be consulted).
    if let Some(obj) = object.as_object()
        && let Some(value) = obj.map_field(slot as usize)
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
    match vm.get_member_name(agent, object, name) {
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

extern "C" fn rmw_compound_computed(
    ctx: *mut c_void,
    object: u64,
    key: u64,
    value: u64,
    op: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let object = Value::from_bits(object);
    let key = Value::from_bits(key);
    let value = Value::from_bits(value);
    let op = ASSIGN_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(AssignOp::AddAssign);
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.compound_member_computed(agent, object, key, op, value) {
        Ok(()) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn rmw_update_computed(ctx: *mut c_void, object: u64, key: u64, op: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let object = Value::from_bits(object);
    let key = Value::from_bits(key);
    let op = UPDATE_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(UpdateOp::Increment);
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.update_member_computed(agent, object, key, op) {
        Ok(()) => Value::Undefined.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn call_slow(
    ctx: *mut c_void,
    callee: u64,
    this: u64,
    argc: u64,
    args: *mut u64,
    direct_eval: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    // G21: a self-call — the callee IS the running closure — runs the
    // compiled body directly, with a nested frame in a private buffer,
    // instead of re-entering the interpreter's call machinery
    // (`do_call_fast` -> `ordinary_call` -> `run_compiled_body` ->
    // `run_jit_body`: a fresh Vm take/return, an execution-context push, a
    // frame setup, a global-shadow walk, a root registration and a whole new
    // ctx) for every recursion level. The identity check is exact — a
    // self-call's callee is the running closure, so its body is this same
    // compiled body (`self_entry` names it). A termination request, a
    // non-eligible body, or the depth cap falls through to the interpreter
    // path below, which surfaces the error or runs interpreted.
    if ctx.self_inline_ok
        && direct_eval == 0
        && ctx.current_function != 0
        && callee == ctx.current_function
        && let Some(result) = self_call_inline(ctx, args, argc)
    {
        return result;
    }
    // A call to a *different* certified body takes the same shape: its machine
    // code runs directly, with a nested frame in a private buffer, instead of
    // the interpreter funnel. `None` (an ineligible or not-yet-compiled callee)
    // falls through to that funnel, which ports the callee and promotes it.
    if direct_eval == 0
        && let Some(result) = certified_call_inline(ctx, callee, this, args, argc)
    {
        return result;
    }
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
    match vm.do_call_fast(agent, argc, direct_eval != 0) {
        Ok(()) => {
            // The general path withdrew the activation instead of performing
            // it in the dispatch frame; this helper's own frame is small, so
            // complete it here before reading the result. An engine builtin
            // that ran in place withdrew nothing, so the check is skipped.
            if vm.has_pending_call()
                && let Err(error) = vm.complete_pending_call(agent)
            {
                vm.stack.truncate(entry_len);
                return slow_error(ctx, error);
            }
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

/// G21: run the running body's compiled entry for a self-call (see
/// [`call_slow`]) — a nested frame carved from a private buffer, the caller's
/// working-area bound and array-index cursor swapped for its duration, and the
/// entry called on the shared ctx. Returns `None` when the call must fall back
/// to the interpreter path (the depth cap, a termination request, or an entry
/// the body does not have).
fn self_call_inline(ctx: &mut JitCallContext, args: *mut u64, argc: u64) -> Option<u64> {
    let agent = unsafe { &mut *ctx.agent };
    if agent.jit_depth >= MAX_JIT_DEPTH || agent.is_terminating() || ctx.self_entry == 0 {
        return None;
    }
    // SAFETY: `ctx.body` is the `Rc<CompiledBody>` the runtime holds for the
    // call's duration (see `JitCallContext::body`), and the machine code is
    // suspended for this synchronous helper.
    let body = unsafe { &*ctx.body };
    let scope = body.scope.as_ref()?;
    // SAFETY: `ctx.self_entry` is the entry `run_jit_body` obtained from the
    // cache for this same body; a fn pointer is pointer-sized.
    let entry: JitEntry = unsafe { std::mem::transmute(ctx.self_entry) };
    let argc = argc as usize;
    // SAFETY: the JIT passes a pointer into its own live stack buffer with
    // `argc` slots (the machine code is suspended for this synchronous call).
    let args = unsafe { std::slice::from_raw_parts(args as *const Value, argc) };
    let frame_len = scope.frame_size;
    let buf_len = frame_len + ctx.self_stack_usage as usize + JIT_STACK_SLACK;
    let (mut inline_buf, mut heap_buf) = ([Value::Undefined; INLINE_JIT_BUF], Vec::<Value>::new());
    let buf: &mut [Value] = if buf_len <= INLINE_JIT_BUF {
        &mut inline_buf[..buf_len]
    } else {
        heap_buf.resize(buf_len, Value::Undefined);
        &mut heap_buf[..]
    };
    // The frame fill mirrors `run_leaf_body`: the params from the call's
    // arguments (missing stay undefined), the TDZ slots uninitialized, the
    // vars undefined. `self_call_eligible` guarantees no `this`/`arguments`
    // slot and no per-call capture context, so the nested run shares the
    // caller's environment unchanged.
    for (slot, cell) in buf.iter_mut().enumerate().take(frame_len) {
        *cell = if slot < scope.arity {
            args.get(slot).copied().unwrap_or(Value::Undefined)
        } else if scope.tdz_store.get(slot).copied().unwrap_or(false) {
            Value::uninitialized()
        } else {
            Value::Undefined
        };
    }
    let frame_ptr = buf.as_mut_ptr() as *mut c_void;
    // SAFETY: `buf` has `frame_len + stack_usage + slack` slots, so the
    // working area starts inside it.
    let stack_ptr = unsafe { buf.as_mut_ptr().add(frame_len) } as *mut c_void;
    // The nested run shares the caller's ctx, so two caller-owned cursors must
    // follow the nested buffer for the call's duration: `buf_end` (a leaf call
    // inside the nested body re-checks its room against it) and the array
    // index stack (a throw mid-literal must not leak an entry into the
    // caller). Both are restored on return.
    let saved_end = ctx.buf_end;
    ctx.buf_end = unsafe { buf.as_mut_ptr().add(buf_len) } as *mut c_void;
    // The nested frame is what the frame-reading Rust helpers must address
    // (`frame_get`); install it for the run and restore the previous value
    // after, so a nested self-call stacks correctly. The shared cursors are
    // `save_scratch`'s. See `certified_call_inline`.
    let saved_nested = unsafe {
        let vm = &mut *ctx.vm;
        let saved = vm.nested_frame;
        vm.nested_frame = Some(buf.as_mut_ptr());
        saved
    };
    // The nested activation shares the caller's `Vm`, so the per-activation
    // scratch registers must not leak either way: a caller mid-`switch`
    // (fall-through), mid-optional-chain, mid-`s += e` append loop, or writing
    // an error span would otherwise have its live values read by the nested
    // body or clobbered by it.
    let saved_scratch = unsafe { (&mut *ctx.vm).save_scratch() };
    // Root the buffer for the call's duration (the Vm is already registered
    // with the active-run tracer, so `run_jit_leaf`'s per-run registration is
    // not needed): a helper it invokes can allocate and trigger a collection,
    // and a heap value only the buffer references must survive until the JIT
    // stores or returns it.
    unsafe {
        (&mut *ctx.vm)
            .jit_roots
            .push((buf.as_ptr() as usize, buf.len()))
    };
    agent.jit_depth += 1;
    // SAFETY: `ctx` is the live per-call context the enclosing compiled body
    // passed to this helper; the entry expects exactly that ABI.
    let result = unsafe {
        entry(
            frame_ptr,
            stack_ptr,
            ctx as *mut JitCallContext as *mut c_void,
        )
    };
    agent.jit_depth -= 1;
    unsafe {
        let vm = &mut *ctx.vm;
        vm.jit_roots.pop();
        vm.nested_frame = saved_nested;
        vm.restore_scratch(saved_scratch);
    }
    ctx.buf_end = saved_end;
    Some(result)
}

/// The general certified-callee lane: a call to a *different* certified body
/// runs its machine code directly on this ctx, with a nested frame in a
/// private buffer, instead of the interpreter funnel (`do_call_fast` ->
/// `ordinary_call` -> `run_compiled_body` -> `run_jit_body`: a pooled-Vm
/// take/reset, an `ExecutionContext` push, a frame setup, a
/// `globals_unshadowed` walk and a whole new ctx). It is [`self_call_inline`]'s
/// shape generalised to any callee — the nested activation shares the caller's
/// `Vm` and control state, and the gate (`self_call_eligible`) is exactly the
/// guarantee that the body reads or writes none of the caller-owned state this
/// lane does not save. Returns `None` when the callee is not an eligible,
/// already-compiled certified body; the caller then takes the interpreter path,
/// which ports the callee and promotes it.
fn certified_call_inline(
    ctx: &mut JitCallContext,
    callee: u64,
    this: u64,
    args: *mut u64,
    argc: u64,
) -> Option<u64> {
    // A callee that is not an ECMAScript function — an engine builtin, a bound
    // function, a proxy — can never be a lane target, and the value decode is
    // the cheapest discriminator, so it comes before the depth/hook reads: the
    // lane probe runs on *every* `call_slow`, and a builtin call (a `Math`/
    // `Map`/`String` method) is the common one to reach here.
    let callee_value = Value::from_bits(callee);
    let ValueKind::Function(function) = callee_value.kind() else {
        return None;
    };
    if !matches!(function.kind, crux::function::FunctionKind::EcmaScript) {
        return None;
    }
    let agent = unsafe { &mut *ctx.agent };
    if agent.jit_depth >= MAX_JIT_DEPTH || agent.is_terminating() {
        return None;
    }
    let hook = agent.jit_hook?;
    // Resolve the callee's registered record in one scoped borrow: a
    // generator/async body's call produces an iterator or a promise rather
    // than a run of the body, and a class constructor cannot be called at all.
    let (body, environment, strict, realm) = {
        let data = agent.ecma_functions.get(&function.id())?;
        if data.is_generator || data.is_async || data.is_class_constructor {
            return None;
        }
        let body = data.ir.clone()?;
        (body, data.environment, data.strict, data.realm)
    };
    let global = realm.global_object;
    // The lane's gate: the self gate minus the `this` requirement (bound
    // below) plus the super/`ThisValue` machinery it cannot install.
    if !body.certified_callee_eligible() {
        return None;
    }
    // An unmapped-`arguments` object is built from the *current* realm's
    // intrinsics (`%Object.prototype%`, `%ThrowTypeError%`), and a lane run
    // pushes no context, so `current_realm` is the caller's: a cross-realm
    // callee would build its object in the wrong realm. Refuse that case — the
    // funnel pushes the callee's realm and is correct.
    if body
        .scope
        .as_ref()
        .is_some_and(|scope| scope.arguments_slot.is_some())
        && !agent
            .current_realm()
            .is_ok_and(|current| current.as_ptr() == realm.as_ptr())
    {
        return None;
    }
    // Compile (or promote) through the same choke point every other JIT site
    // uses; a below-threshold or sticky-refused body falls back here.
    let info_ptr = lookup_info(hook, &body, true);
    if info_ptr.is_null() {
        return None;
    }
    // SAFETY: `lookup_info` just returned the cache's live entry, and no frame
    // is in flight to evict it (`in_flight` was true).
    let info = unsafe { &*info_ptr };
    let scope = body.scope.as_ref()?;
    // SAFETY: `info.entry` is a code pointer the cache owns.
    let entry: JitEntry = unsafe { std::mem::transmute(info.entry) };
    let argc = argc as usize;
    // SAFETY: the JIT passes a pointer into its own live stack buffer with
    // `argc` slots (the machine code is suspended for this synchronous call).
    let args = unsafe { std::slice::from_raw_parts(args as *const Value, argc) };
    let frame_len = scope.frame_size;
    let buf_len = frame_len + info.stack_usage + JIT_STACK_SLACK;
    let (mut inline_buf, mut heap_buf) = ([Value::Undefined; INLINE_JIT_BUF], Vec::<Value>::new());
    let buf: &mut [Value] = if buf_len <= INLINE_JIT_BUF {
        &mut inline_buf[..buf_len]
    } else {
        heap_buf.resize(buf_len, Value::Undefined);
        &mut heap_buf[..]
    };
    // The frame fill mirrors `setup_certified_frame` for a gated body: params
    // from the call's arguments (missing stay undefined), TDZ slots
    // uninitialized, `var`s undefined. `self_call_eligible` guarantees no
    // `this`/`arguments` slot and no per-call capture context.
    for (slot, cell) in buf.iter_mut().enumerate().take(frame_len) {
        *cell = if slot < scope.arity {
            args.get(slot).copied().unwrap_or(Value::Undefined)
        } else if scope.tdz_store.get(slot).copied().unwrap_or(false) {
            Value::uninitialized()
        } else {
            Value::Undefined
        };
    }
    // OrdinaryCallBindThis (spec 10.2.1.1) into the callee's `this` slot — the
    // same bind `setup_certified_frame` is handed by `ordinary_call`: strict
    // keeps the call's `this` as-is; sloppy coerces a nullish `this` to the
    // callee realm's global object and passes an object/function through. A
    // primitive receiver must be boxed (`to_object` allocates and can throw),
    // so that case falls back to the funnel instead.
    if let Some(slot) = scope.this_slot {
        let this = Value::from_bits(this);
        buf[slot] = if strict {
            this
        } else {
            match this.kind() {
                ValueKind::Undefined | ValueKind::Null => Value::Object(global),
                ValueKind::Object(_) | ValueKind::Function(_) => this,
                _ => return None,
            }
        };
    }
    let frame_ptr = buf.as_mut_ptr() as *mut c_void;
    // SAFETY: `buf` has `frame_len + stack_usage + slack` slots.
    let stack_ptr = unsafe { buf.as_mut_ptr().add(frame_len) } as *mut c_void;
    let (apply_builtin_bits, call_builtin_bits) = if body.has_call_apply {
        call_apply_intrinsic_bits(agent)
    } else {
        (0, 0)
    };
    let intrinsic_bits = if body.has_call_intrinsic {
        intrinsic_bits(agent)
    } else {
        [0; INTRINSIC_COUNT]
    };
    // The nested run reads the callee's environment, function and strictness
    // from the shared `Vm`, and a *different* body may use the shared scratch
    // registers (the `switch` discriminant, the statement-completion register,
    // the optional-chain short flag, the string builder, the register
    // accumulator, `ip`); `save_scratch` saves and resets them and
    // `restore_scratch` puts the caller's back, so neither run sees the
    // other's.
    let (scratch, saved) = unsafe {
        let vm = &mut *ctx.vm;
        let scratch = vm.save_scratch();
        let saved = (
            vm.lexical_env,
            vm.body_context,
            vm.current_function,
            vm.current_new_target,
            vm.strict,
        );
        vm.lexical_env = environment;
        vm.body_context = Some(environment);
        vm.current_function = Some(callee_value);
        vm.current_new_target = None;
        vm.strict = strict;
        (scratch, saved)
    };
    // The frame-reading Rust helpers (`builder_bind`/`builder_store`,
    // `create_function_decl`) resolve `frame_get` through `nested_frame`, so
    // install the callee's buffer for the run — saving the previous value, so a
    // nested lane run stacks correctly. The shared cursors a nested run must
    // not leak into are `save_scratch`'s. An unmapped-`arguments` body reads
    // the call's argument slice through `Vm::call_args`, so set that too
    // (mirroring `run_leaf_body`); the mapped (sloppy) form is refused by the
    // gate.
    let (saved_nested, saved_call_args) = unsafe {
        let vm = &mut *ctx.vm;
        let saved = vm.nested_frame;
        vm.nested_frame = Some(buf.as_mut_ptr());
        let call_args = if scope.arguments_slot.is_some() {
            Some(std::mem::replace(&mut vm.call_args, args.to_vec()))
        } else {
            None
        };
        (saved, call_args)
    };
    // Root the buffer for the call's duration (the Vm is already registered
    // with the active-run tracer): a helper it invokes can allocate and
    // trigger a collection, and a heap value only the buffer references must
    // survive until the JIT stores or returns it.
    unsafe {
        (&mut *ctx.vm)
            .jit_roots
            .push((buf.as_ptr() as usize, buf.len()))
    };
    let global_bits = Value::Object(global).bits();
    let mut nested = JitCallContext {
        pending: false,
        error: None,
        agent: ctx.agent,
        vm: ctx.vm,
        global_object: global.as_ptr() as *mut c_void,
        global_value_cells: ctx.global_value_cells,
        member_value_cells: ctx.member_value_cells,
        computed_read_cells: ctx.computed_read_cells,
        member_map_cells: ctx.member_map_cells,
        globals_unshadowed: Vm::global_reads_are_unshadowed(environment, &body.ident_names),
        buf_end: (buf.as_ptr() as usize + buf_len * std::mem::size_of::<Value>()) as *mut c_void,
        leaf_epoch: 0,
        leaf_records: ctx.leaf_records,
        leaf_gen: ctx.leaf_gen,
        body: std::rc::Rc::as_ptr(&body),
        tail: false,
        current_function: callee,
        self_entry: info.entry as u64,
        self_stack_usage: info.stack_usage as u64,
        self_inline_ok: true,
        apply_builtin_bits,
        call_builtin_bits,
        intrinsic_bits,
        dispatch_value: 0,
        suspension: None,
        suspend_sp: 0,
        global_bits,
        resume_kind: 0,
        resume_ip: 0,
        resume_sp: 0,
        resume_value: 0,
        gc_ticks: JIT_GC_PROBE_INTERVAL,
    };
    agent.jit_depth += 1;
    // SAFETY: `nested` is the live per-call context the entry expects.
    let result = unsafe {
        entry(
            frame_ptr,
            stack_ptr,
            (&mut nested as *mut JitCallContext) as *mut c_void,
        )
    };
    agent.jit_depth -= 1;
    unsafe {
        let vm = &mut *ctx.vm;
        vm.jit_roots.pop();
        vm.nested_frame = saved_nested;
        if let Some(call_args) = saved_call_args {
            vm.call_args = call_args;
        }
        (
            vm.lexical_env,
            vm.body_context,
            vm.current_function,
            vm.current_new_target,
            vm.strict,
        ) = saved;
        vm.restore_scratch(scratch);
    }
    if nested.pending {
        return Some(slow_error(
            ctx,
            nested.error.take().expect("a pending JIT error is present"),
        ));
    }
    debug_assert!(
        !nested.tail,
        "the certified-call gate excludes the tail steps that set ctx.tail"
    );
    debug_assert!(
        result != DISPATCH_SUSPEND,
        "the certified-call gate excludes the suspend steps"
    );
    Some(result)
}

extern "C" fn call_apply(
    ctx: *mut c_void,
    resolved: u64,
    callee: u64,
    argc: u64,
    args: *mut u64,
    kind: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let argc = argc as usize;
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let entry_len = vm.stack.len();
    // The `[f, apply/call, thisArg, arg1..argN]` layout `do_call_apply`
    // expects, pushed from the JIT buffer (see `call_slow` — the buffer is
    // stable for the synchronous helper).
    vm.stack.push(Value::from_bits(callee));
    vm.stack.push(Value::from_bits(resolved));
    for i in 0..argc {
        // SAFETY: the JIT passes a pointer into its own (live) stack buffer
        // with `argc` slots.
        vm.stack.push(Value::from_bits(unsafe { *args.add(i) }));
    }
    let kind = if kind == 0 {
        crate::ir::ApplyKind::Apply
    } else {
        crate::ir::ApplyKind::Call
    };
    match vm.do_call_apply(agent, argc, kind) {
        Ok(()) => {
            // The general path (a shadowed apply/call or a non-leaf callee)
            // withdrew the activation; complete it here (see `call_slow`).
            if vm.has_pending_call()
                && let Err(error) = vm.complete_pending_call(agent)
            {
                vm.stack.truncate(entry_len);
                return slow_error(ctx, error);
            }
            let result = match vm.stack.pop() {
                Some(value) => value,
                None => {
                    vm.stack.truncate(entry_len);
                    return slow_error(
                        ctx,
                        JsError::new(
                            ErrorKind::TypeError,
                            "the JIT apply call produced no result".into(),
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

/// M10: the compiled `Step::CallApply` fast path's dense-`argArray` element
/// fill — the one remaining heap read (the array's element buffer) the
/// machine code cannot do inline. When `arg_array` is a dense Array whose
/// whole `[0, length)` range is present, copies its element bits into the
/// JIT buffer at `dest` and returns the element count; `u64::MAX` when the
/// array is not a dense fast Array, is longer than `JIT_APPLY_MAX_ARGS`, or
/// would not fit before the buffer's end. Nothing is written on the reject
/// paths, so the caller's fallback (`call_apply` with the untouched region)
/// stays safe. Mirrors `do_call_apply`'s dense fast path (the machine code
/// is suspended for the synchronous helper, so the buffer is stable, and
/// the array itself roots the copied values).
extern "C" fn apply_args_fill(ctx: *mut c_void, arg_array: u64, dest: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let arg_array = Value::from_bits(arg_array);
    let ValueKind::Object(obj) = arg_array.kind() else {
        return u64::MAX;
    };
    let crux::object::ObjectKind::Array(slots) = &obj.kind else {
        return u64::MAX;
    };
    if !slots.dense.get() {
        return u64::MAX;
    }
    let length = slots.length.get() as usize;
    if length > JIT_APPLY_MAX_ARGS {
        return u64::MAX;
    }
    let capacity = ((ctx.buf_end as usize).saturating_sub(dest as usize)) / 8;
    if length > capacity {
        return u64::MAX;
    }
    let elements = slots.elements();
    // The whole range must be present before any write: a hole discovered
    // mid-copy must not leave a partial region the slow path would read.
    if elements.len() < length || !elements[..length].iter().all(|value| !value.is_hole()) {
        return u64::MAX;
    }
    let dest = dest as *mut u64;
    for (index, element) in elements[..length].iter().enumerate() {
        // SAFETY: the capacity check above guarantees `length` slots fit.
        unsafe { *dest.add(index) = element.bits() };
    }
    length as u64
}

/// The per-slot TDZ source for a frame fill: the call record's mask (the
/// compiled hit path, which has no scope) or the scope's own store (the probe,
/// which can describe a frame wider than the mask).
enum TdzSource<'a> {
    Mask(u64),
    Store(&'a Vec<bool>),
}

impl TdzSource<'_> {
    #[inline]
    fn is_uninitialized(&self, slot: usize) -> bool {
        match self {
            TdzSource::Mask(mask) => mask >> slot & 1 == 1,
            TdzSource::Store(store) => store.get(slot).copied().unwrap_or(false),
        }
    }
}

/// Build the record's fill fields for `scope`: the `this` slot, strictness and
/// the TDZ mask. `fill_ok` is 0 when the frame is wider than `TDZ_MASK_SLOTS`,
/// which keeps the compiled hit path on the probe (the probe still fills such a
/// frame from the scope itself).
fn leaf_fill_info(
    scope: &ScopeInfo,
    strict: bool,
    entry: u64,
    stack_usage: usize,
    uses_env: bool,
) -> LeafInlineInfo {
    let this_slot = scope.this_slot.map_or(NO_THIS_SLOT, |slot| slot as u32);
    let (tdz_mask, fill_ok) = if scope.frame_size > TDZ_MASK_SLOTS {
        (0, 0)
    } else {
        let mut tdz_mask = 0;
        for slot in 0..scope.frame_size {
            if scope.tdz_store.get(slot).copied().unwrap_or(false) {
                tdz_mask |= 1u64 << slot;
            }
        }
        (tdz_mask, 1)
    };
    LeafInlineInfo {
        entry,
        stack_usage: stack_usage as u64,
        frame_size: scope.frame_size as u32,
        arity: scope.arity as u32,
        this_slot,
        strict: u32::from(strict),
        tdz_mask,
        fill_ok,
        uses_env: u32::from(uses_env),
    }
}

/// G14: rebuild a leaf's inline frame above the argument region — bind the
/// receiver per `OrdinaryCallBindThis` (a strict callee keeps the receiver, a
/// sloppy object keeps it too, a sloppy nullish one becomes the realm's global
/// object — `global_bits`, the ctx snapshot of the running realm's global — and
/// a sloppy primitive one BOXES, an allocation these synchronous helpers
/// cannot do, so it refuses), check the frame + working area fits in the
/// caller's buffer, then write the slots: params copied from the arguments
/// (missing ones `undefined`), `var` slots `undefined`, lexical slots the TDZ
/// marker, the `this` slot the bound receiver. The aliased frame IS the
/// argument region, so only its working area is checked and nothing is
/// written. Shared by `leaf_call_probe` (a site's first visit, which fills from
/// the scope) and `leaf_call_fill` (the compiled hit path, which fills from the
/// recorded descriptor), so both produce the identical frame and reject on the
/// identical conditions. Returns false when the receiver cannot be bound or the
/// area does not fit.
#[inline]
fn fill_leaf_frame(
    info: &LeafInlineInfo,
    tdz: &TdzSource<'_>,
    this: u64,
    args: *mut u64,
    argc: usize,
    buf_end: usize,
    global_bits: u64,
) -> bool {
    let frame_size = info.frame_size as usize;
    let arity = info.arity as usize;
    let bound_this = if info.this_slot == NO_THIS_SLOT {
        0
    } else {
        let receiver = Value::from_bits(this);
        if info.strict != 0 {
            receiver.bits()
        } else {
            match receiver.kind() {
                ValueKind::Object(_) | ValueKind::Function(_) => receiver.bits(),
                ValueKind::Undefined | ValueKind::Null => global_bits,
                _ => return false,
            }
        }
    };
    // The inline frame + working area must fit above the argument region's
    // top in the caller's working buffer: the aliased case (the frame IS the
    // arguments) needs only the working area; the built frame adds its
    // frame_size slots on top of the args.
    let aliased = frame_size == arity && argc >= frame_size;
    let args_top = (args as usize) + argc * 8;
    let needed = (if aliased { 0 } else { frame_size }) + info.stack_usage as usize;
    if args_top + needed * 8 > buf_end {
        return false;
    }
    // Fill the leaf's frame above the arguments (the aliased case is the
    // arguments themselves — no fill; missing arguments stay `undefined`,
    // var slots `undefined`, lexical slots the uninitialized marker). The
    // buffer is only written by the machine code, which is suspended for
    // the duration of these synchronous helpers.
    if !aliased {
        let frame = args_top as *mut u64;
        for slot in 0..frame_size {
            let value = if slot as u32 == info.this_slot {
                bound_this
            } else if slot < arity {
                if slot < argc {
                    // SAFETY: the caller passes a pointer into its own (live)
                    // stack buffer with `argc` slots.
                    unsafe { *args.add(slot) }
                } else {
                    Value::Undefined.bits()
                }
            } else if tdz.is_uninitialized(slot) {
                Value::uninitialized().bits()
            } else {
                Value::Undefined.bits()
            };
            // SAFETY: the room check above guarantees `frame_size` slots
            // fit past the argument region's top.
            unsafe { *frame.add(slot) = value };
        }
    }
    true
}

/// G15: the masked record index the compiled call site selected. The machine
/// code computed `leaf_record_slot(callee)`; the mask is defensive, so a
/// corrupted argument can never index out of bounds. A wrong slot is caught by
/// [`record_matches`], never applied.
#[inline]
fn record_slot_of(callee: u64, slot: u64) -> usize {
    debug_assert_eq!(slot as usize, leaf_record_slot(callee));
    (slot as usize) & (LEAF_CALL_RECORDS - 1)
}

/// G15: whether `record` still describes `callee` on this run — the code
/// generation matches (the callee's compiled entry was not evicted since the
/// probe) and the exact NaN-box identity matches. The helpers re-check what
/// the machine code already validated, so a stale or collided record is
/// rejected rather than applied.
#[inline]
fn record_matches(ctx: &JitCallContext, record: &LeafCallRecord, callee: u64) -> bool {
    record.code_gen == ctx.leaf_gen
        && record.callee_hi == (callee >> 44) as u32
        && record.callee_payload == callee & crux::PAYLOAD_MASK
}

/// G14: the compiled leaf-call *hit* path for a non-aliased frame. The
/// machine code calls this only after its record gate matched the callee's
/// identity, the code generation and the live leaf-eligibility epoch, so the
/// verdict is already known and the fill descriptor was recorded by the probe:
/// this rebuilds the callee's frame straight from the record — no
/// `leaf_lookup`, no `Rc` clone, no eligibility re-validation — and returns the
/// cached entry. Returns 0 when the frame no longer fits, so the site takes
/// `call_slow`.
extern "C" fn leaf_call_fill(
    ctx: *mut c_void,
    callee: u64,
    this: u64,
    args: *mut u64,
    argc: u64,
    slot: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let cache_entry = unsafe { &*ctx.leaf_records.add(record_slot_of(callee, slot)) };
    let info = &cache_entry.leaf_inline;
    // Defense in depth: the machine code validated the record's generation,
    // the callee identity and the `fill_ok` gate before branching here, and
    // no JS runs between that gate and this call.
    if !record_matches(ctx, cache_entry, callee) || info.entry == 0 || info.fill_ok == 0 {
        return 0;
    }
    if !fill_leaf_frame(
        info,
        &TdzSource::Mask(info.tdz_mask),
        this,
        args,
        argc as usize,
        ctx.buf_end as usize,
        ctx.global_bits,
    ) {
        return 0;
    }
    info.entry
}

extern "C" fn leaf_call_probe(
    ctx: *mut c_void,
    callee: u64,
    this: u64,
    args: *mut u64,
    argc: u64,
    slot: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    // SAFETY: `record_slot_of` masks below `LEAF_CALL_RECORDS`, and
    // `leaf_records` is the running agent's table, live for this run.
    let cache_entry = unsafe { &mut *ctx.leaf_records.add(record_slot_of(callee, slot)) };
    // Record the record identity up front, before any rejection: the compiled
    // code reuses this record only when its code generation, the live leaf
    // epoch, and the callee's full NaN-box identity all match — so a cached
    // zero entry skips the probe for a stable rejection, and a stale record is
    // simply re-probed. The one rejection that is not stable — a body the
    // tier-up policy has not compiled yet — clears the identity instead (see
    // `lookup_info`'s null arm below), so the next visit re-probes.
    *cache_entry = LeafCallRecord {
        callee_payload: callee & crux::PAYLOAD_MASK,
        leaf_inline: LeafInlineInfo::empty(),
        epoch: ctx.leaf_epoch,
        code_gen: ctx.leaf_gen,
        callee_hi: (callee >> 44) as u32,
    };
    let callee = Value::from_bits(callee);
    // The eligibility mirrors `fast_call_core`'s leaf gate (the compiled
    // call site is inside a certified body, whose own stacks are the ones
    // `can_inline_leaf` checks). G12/G13 lifted the old `this`-slot and
    // environment refusals: a `this` slot is filled by `fill_leaf_frame` (the
    // probe binds the receiver here, the hit path rebuilds it from the
    // record), and an environment-reading leaf is recorded with `uses_env` so
    // the compiled hit path routes it to the env lane (`leaf_call_env`)
    // instead of calling it in-frame.
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
    // Copy the fields out before `ir` is cloned: `leaf_lookup` returns a borrow
    // of `agent`, and `strict` is needed much later (the `this` binding), so the
    // borrow must end here.
    let strict = entry.strict;
    let ir = entry.ir.clone();
    let Some(scope) = ir.scope.as_ref() else {
        return 0;
    };
    // A body with a `this` slot is inlineable too — the slot is filled below
    // (see `bound_this`). Its `this` reads lower to `LoadLocal { this_slot }`
    // (a frame read, which is leaf-eligible); a body that reaches
    // `Step::ThisValue` instead is not leaf-certified at all.
    // The leaf must have compiled machine code (compiling on first use); a
    // body without compiled code falls back to `call_slow`, whose
    // interpreter leaf-inline path handles it. The in-flight flag keeps the
    // cache from evicting a frame running right now.
    let Some(hook) = agent.jit_hook else {
        return 0;
    };
    let info_ptr = crate::jit::lookup_info(hook, &ir, agent.jit_depth > 0);
    if info_ptr.is_null() {
        // Two different nulls. A body the compile threshold has not promoted
        // yet still counts the consult above toward that threshold, so a later
        // visit can succeed; a body the policy has refused for good (over the
        // step cap, or an emitter refusal) is sticky, which `jit_info`'s `1`
        // marks. Only the first is transient, so only it must not leave a
        // cached rejection behind: the compiled call site would otherwise
        // reuse "not inlineable" forever and the threshold could never be
        // reached — the reason every straight-line leaf call stayed on
        // `call_slow`.
        if ir.jit_info.get() != 1 {
            *cache_entry = LeafCallRecord::empty();
        }
        return 0;
    }
    let compiled = unsafe { &*info_ptr };
    // G13: a body that reads its environment (context-slot steps, or the
    // per-iteration reads) is a certified leaf like any other — the
    // interpreter's own leaf gate (`fast_call_core`) has no env condition, and
    // `run_jit_leaf` performs the swap. The MACHINE code cannot: the
    // `body_context`/`lexical_env` swap would have to span the in-frame call,
    // including its error exit. So the probe records the descriptor with
    // `uses_env` set and refuses, and the compiled hit path's env lane
    // (`leaf_call_env`) owns the whole call.
    let uses_env = ir.leaf_uses_env;
    let info = leaf_fill_info(
        scope,
        strict,
        compiled.entry as u64,
        compiled.stack_usage,
        uses_env,
    );
    // The frame fill (bind `this`, room check, write slots) is identical to
    // the compiled hit path's (G14) — see `fill_leaf_frame`; a rejection there
    // is the same rejection here. The probe fills from the scope itself, and
    // records the descriptor the hit path fills from, so the two cannot drift.
    // An env leaf is not filled here: the env lane fills from the record
    // before it swaps the env in, so a wide frame (no usable mask) must not be
    // recorded as env-inlineable either.
    let mut info = info;
    if uses_env {
        if info.fill_ok == 0 {
            info.entry = 0;
        }
    } else if !fill_leaf_frame(
        &info,
        &TdzSource::Store(&scope.tdz_store),
        this,
        args,
        argc as usize,
        ctx.buf_end as usize,
        ctx.global_bits,
    ) {
        return 0;
    }
    cache_entry.leaf_inline = info;
    if uses_env {
        return 0;
    }
    compiled.entry as u64
}

/// G13: run a certified leaf that READS ITS ENVIRONMENT on the caller's live
/// `JitCallContext` and working buffer. The machine code cannot: the
/// `body_context`/`lexical_env` swap has to span the in-frame call, including
/// its error exit, and the leaf runs on the caller's ctx, so both must be put
/// back before the caller resumes. The frame was already rebuilt in the
/// caller's buffer by `leaf_call_fill` (the compiler's env lane calls it
/// first), so this only re-derives the callee's environment, swaps it in,
/// calls the compiled entry and restores — mirroring `run_jit_leaf`'s env
/// handling and tail, in the same order, so the two cannot diverge. Returns
/// the result bits, or `u64::MAX` (which no `Value` encoding uses) when the
/// site must fall back to `call_slow`.
extern "C" fn leaf_call_env(
    ctx: *mut c_void,
    callee: u64,
    args: *mut u64,
    argc: u64,
    slot: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let cache_entry = unsafe { &*ctx.leaf_records.add(record_slot_of(callee, slot)) };
    let info = &cache_entry.leaf_inline;
    // Defense in depth: the machine code validated the record's generation,
    // the callee identity and the `uses_env` gate before branching here, and
    // no JS runs between that gate and this call.
    if !record_matches(ctx, cache_entry, callee)
        || info.entry == 0
        || info.uses_env == 0
        || info.fill_ok == 0
    {
        return u64::MAX;
    }
    let entry = info.entry;
    let frame_size = info.frame_size as usize;
    let arity = info.arity as usize;
    if agent.jit_depth >= MAX_JIT_DEPTH {
        return u64::MAX;
    }
    let callee = Value::from_bits(callee);
    let ValueKind::Function(function) = callee.kind() else {
        return u64::MAX;
    };
    let Some(leaf) = agent.leaf_lookup(function.id()) else {
        return u64::MAX;
    };
    // An env leaf always carries its closure environment (`leaf_lookup` sets
    // it from `leaf_uses_env`); without one the swap would install nothing.
    let Some(environment) = leaf.environment else {
        return u64::MAX;
    };
    let strict = leaf.strict;
    let ir = leaf.ir.clone();
    let Some(scope) = ir.scope.as_ref() else {
        return u64::MAX;
    };
    let _ = strict;
    let argc = argc as usize;
    let aliased = frame_size == arity && argc >= frame_size;
    let frame_base = if aliased {
        args
    } else {
        // SAFETY: the caller's machine code pushed `argc` argument slots at
        // `args`, and `leaf_call_fill` already checked the frame fits above
        // them (the room check the in-frame lane also relies on).
        unsafe { args.add(argc) }
    };
    // SAFETY: `args` points at `argc` live argument slots in the caller's
    // buffer, and `Value` is `#[repr(transparent)]` over `u64`.
    let args_slice = unsafe { std::slice::from_raw_parts(args as *const Value, argc) };
    let Ok(body_env) = scope.new_body_context(&environment, args_slice) else {
        return u64::MAX;
    };
    let body_env = body_env.unwrap_or(environment);
    let caller_body_context = vm.body_context.replace(body_env);
    let caller_lexical_env = if ir.leaf_needs_env {
        Some(std::mem::replace(&mut vm.lexical_env, body_env))
    } else {
        None
    };
    // The frame lives in the caller's rooted working buffer, so a heap value it
    // holds only the buffer references survives a collection a helper inside
    // the leaf may trigger.
    let stack_len = vm.stack.len();
    let array_index_stack_len = vm.array_index_stack.len();
    let stack_ptr = unsafe { frame_base.add(frame_size) } as *mut c_void;
    let ctx_ptr = ctx as *mut JitCallContext as *mut c_void;
    // SAFETY: `entry` is the leaf's compiled entry (the probe recorded it) with
    // the `JitEntry` ABI; the frame and working area above `frame_base` were
    // sized and rooted by the caller's buffer.
    let entry: JitEntry = unsafe { std::mem::transmute(entry as usize) };
    agent.jit_depth += 1;
    let result = unsafe { (entry)(frame_base as *mut c_void, stack_ptr, ctx_ptr) };
    agent.jit_depth -= 1;
    // `run_jit_leaf`'s tail, in the same order: drop anything the leaf's
    // helpers left transient, put the caller's environment back, then let the
    // machine code's pending check surface a throw (the caller's ctx is the
    // one this leaf ran on, so `pending` is already its own).
    vm.stack.truncate(stack_len);
    vm.array_index_stack.truncate(array_index_stack_len);
    if let Some(saved_env) = caller_body_context {
        vm.body_context = Some(saved_env);
    }
    if let Some(saved_env) = caller_lexical_env {
        vm.lexical_env = saved_env;
    }
    result
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
    let reference =
        match crate::context::resolve_binding_from(vm.lexical_env, &name_string, vm.strict) {
            Ok(reference) => reference,
            Err(error) => return slow_error(ctx, error),
        };
    let value = match crate::context::get_value(agent, &reference) {
        Ok(value) => value,
        Err(error) => return slow_error(ctx, error),
    };
    // Warm the JIT's global fast cell when the resolved binding is a global
    // environment-record binding — the compiled `LoadIdent` probe then serves
    // the next read as a native load. An OBJECT-record binding
    // (var/function/undeclared) records with its property slot, so a compiled
    // `StoreGlobal` can write through the cell too; a DECLARATIVE-record
    // binding (a top-level `let`/`const`/`class`) records load-only and is
    // invalidated by the generation bump the global env performs on every
    // declarative mutation. Any other env never warms. Best-effort — a name
    // whose shape does not fit (an accessor, an absent property) stays
    // missing and the resolve path keeps running.
    if let ReferenceBase::Environment(env) = &reference.base
        && let EnvRecord::Global(_) = &**env
    {
        let declarative = env.has_lexical_declaration(&name_string);
        vm.warm_global_cell(agent, name_atom, value, declarative);
    }
    value.bits()
}

/// `Step::TypeofIdent`: `typeof` of a name that resolves through the
/// environment chain — the `BindingLoc::Env` form `compile_expr` emits when the
/// name is neither a frame slot, a capture-context slot, the accumulator, nor a
/// global fast binding. Spec 13.5.3.2 step 1 is why this is its own helper
/// rather than `load_ident` + `typeof_top`: an UNRESOLVABLE reference must
/// answer `"undefined"` and not throw, and `load_ident` errors on it. Resolving
/// and reading can re-enter user code (an accessor on the global object), so a
/// failure reports through `slow_error` and the caller's leaf-epoch bump
/// applies. It deliberately does NOT warm the global-value cell the way
/// `load_ident` does: no compiled probe reads a `typeof` of a name, so there is
/// no cell to serve.
extern "C" fn typeof_ident(ctx: *mut c_void, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let reference = match crate::context::resolve_binding_from(
        vm.lexical_env,
        &crux::lookup(name as crux::AtomId),
        vm.strict,
    ) {
        Ok(reference) => reference,
        Err(error) => return slow_error(ctx, error),
    };
    let value = match &reference.base {
        ReferenceBase::Unresolvable => Value::Undefined,
        _ => match crate::context::get_value(agent, &reference) {
            Ok(value) => value,
            Err(error) => return slow_error(ctx, error),
        },
    };
    typeof_bits(&value)
}

extern "C" fn resolve_var_ident(ctx: *mut c_void, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    match crate::context::resolve_binding_from(
        vm.lexical_env,
        &crux::lookup(name as crux::AtomId),
        vm.strict,
    ) {
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
    let reference = match crate::context::resolve_binding_from(
        vm.lexical_env,
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
        // A GLOBAL object is additionally read through the name-keyed
        // global-value cell, which validates by this same generation — the one
        // thing this in-place store deliberately does not bump (see
        // `Vm::refresh_global_read_cell`).
        Vm::refresh_global_read_cell(agent, &obj, name, value);
        return value.bits();
    }
    // The narrow write declined. The common case on a FRESH constructor
    // `this` is a map-described hole (the shape gate hit pins the map, not
    // presence): the store is a fresh define, which the interpreter's lean
    // `fast_fresh_store` serves after the warm-store probe. Running the
    // full assign machinery here (not a straight [[Set]]) keeps that
    // constructor fill on the fast fresh-define route; the remaining
    // declines (a non-writable own property, an accessor-converted chain,
    // an exotic receiver) land in the same full [[Set]] the assign path
    // uses. `assign_member` pushed the result; pop it back so the value
    // stack stays balanced across the JIT body's helpers.
    match vm.assign_member(
        agent,
        object,
        crate::ir::PropertyKeyName::Name(name),
        None,
        value,
        syntax::ast::AssignOp::Assign,
    ) {
        Ok(()) => match vm.stack.pop() {
            Some(result) => result.bits(),
            None => value.bits(),
        },
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

extern "C" fn fast_array_element_write(
    _ctx: *mut c_void,
    object: u64,
    key: u64,
    value: u64,
) -> u64 {
    // The JIT's inline dense-array store: mirror exactly the fast path of
    // `assign_computed_plain` (the Array kind check, the canonical index
    // Number check, then `array_element_write`). 1 = stored, 0 = fall back
    // to the full `assign_member_computed` helper (which re-runs the checks
    // and the [[Set]] machinery, including the nullish error). The helper
    // never sets the pending byte: the chain walk bails on any link that
    // is not a plain Ordinary/Array object, so no proxy trap or getter can
    // run, and `array_element_write` on that shape cannot error.
    let object = Value::from_bits(object);
    let ValueKind::Object(obj) = object.kind() else {
        return 0;
    };
    let ValueKind::Number(number) = Value::from_bits(key).kind() else {
        return 0;
    };
    if number.fract() != 0.0 || !(0.0..4294967295.0).contains(&number) {
        return 0;
    }
    let value = Value::from_bits(value);
    // A typed array stores by numeric index through `typed_array_element_set`
    // (IntegerIndexedElementSet — the same path `assign_computed_plain`
    // takes), skipping the key-string conversion the general helper would
    // do. Only a PRIMITIVE value is accepted: the element coercion
    // (ToNumber/ToBigInt) of an object can run user code (toPrimitive), and
    // the helper must not set the pending byte. The fallback re-runs the
    // coercion and throws the identical TypeError for a wrong content type
    // (BigInt on a Number array, ...) — nothing observable ran on the fast
    // attempt, so re-running is safe.
    if let crux::object::ObjectKind::IntegerIndexed(slots) = &obj.kind {
        if matches!(value.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
            return 0;
        }
        return match obj.typed_array_element_set(slots, number as u64, value) {
            Ok(true) => 1,
            _ => 0,
        };
    }
    if !matches!(obj.kind, crux::object::ObjectKind::Array(_)) {
        return 0;
    }
    match obj.array_element_write(number as u64, value) {
        Ok(Some(())) => 1,
        _ => 0,
    }
}

extern "C" fn dense_array_append(_ctx: *mut c_void, object: u64, index: u64, value: u64) -> u64 {
    // The JIT-inline dense append (gap-close M1 C): the compiled gate
    // verified the receiver is a dense Array and the key a canonical index
    // Number equal to `slots.length`; re-verify the dense gate cheaply,
    // then delegate the append (extensibility, chain-clean, buffer push,
    // length write + mirror, generation bump) to the shared
    // `array_element_write` — index == length takes its append branch, and
    // any deviation returns 0 (the caller re-runs the full [[Set]]
    // machinery; nothing observable ran on the fast attempt, so the
    // fallback is correct). Never sets the pending byte.
    let object = Value::from_bits(object);
    let ValueKind::Object(obj) = object.kind() else {
        return 0;
    };
    if obj.array_dense.get().is_none() {
        return 0;
    }
    match obj.array_element_write(index, Value::from_bits(value)) {
        Ok(Some(())) => 1,
        _ => 0,
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
    env.set_slot(index as usize, value);
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
        span,
    }) = step_at(ctx, step)
    else {
        unreachable!("create_arrow on a non-CreateArrow step");
    };
    let env = vm.lexical_env;
    match crate::function::instantiate_arrow(
        agent,
        *is_async,
        params.as_slice(),
        body,
        env,
        *strict,
        outer_chain.clone(),
        per_iteration_chain.clone(),
        *span,
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
        env.set_slot(*index, value);
    } else {
        unreachable!("FunctionDeclInit without a binding slot (the scan allocated one)");
    }
    value.bits()
}

extern "C" fn new_target(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    // Cut 62: the certified path's `new.target` is the per-run Vm field
    // (the frame-slot model has no FunctionEnv); the env path reads the
    // running this-environment's binding.
    let value = if vm.body_context.is_some() {
        vm.current_new_target.unwrap_or(Value::Undefined)
    } else {
        match crate::context::get_new_target(agent) {
            Ok(value) => value,
            Err(error) => return slow_error(ctx, error),
        }
    };
    value.bits()
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

extern "C" fn tagged_template(ctx: *mut c_void, tag: u64, this: u64, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let Some(crate::ir::Step::TaggedTemplate(template)) = step_at(ctx, step) else {
        unreachable!("tagged_template on a non-TaggedTemplate step");
    };
    // Mirror the interpreter's `Step::TaggedTemplate` handler: pop the
    // argument boundary and split the substitutions (built by the compiled
    // ArgsBase/ArgsPush steps into the Vm's vector), then run the tag. The
    // substitution Vec is unrooted while the tag invocation allocates, so
    // `--gc-stress` is suppressed for the window (like the handler).
    let base = match vm.args_base_stack.pop() {
        Some(base) => base,
        None => {
            return slow_error(
                ctx,
                JsError::new(
                    ErrorKind::SyntaxError,
                    "TaggedTemplate without an argument boundary".into(),
                ),
            );
        }
    };
    let _stress = crate::ir::StressSuppress::new();
    let substitutions = vm.args.split_off(base);
    match crate::ir::tagged_template(
        agent,
        Value::from_bits(this),
        Value::from_bits(tag),
        template,
        substitutions,
    ) {
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
            // The general path withdrew the activation; complete it here (see
            // `call_slow`).
            if vm.has_pending_call()
                && let Err(error) = vm.complete_pending_call(agent)
            {
                vm.stack.truncate(entry_len);
                return slow_error(ctx, error);
            }
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

extern "C" fn construct(ctx_raw: *mut c_void, callee: u64, sp: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx_raw) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    // Mirror the interpreter's `Step::Construct`: pop the argument
    // boundary and run the construct-inline leaf fast path or the general
    // construct machinery, returning the constructed value (the machine
    // code pushes it onto the work stack). The caller's ctx + current
    // working `sp` ride along so an environment-free leaf body can run on
    // the caller's ctx with a frame carved from its buffer (no per-construct
    // ctx rebuild) — see `step_construct_shared`.
    match vm.step_construct_shared(agent, Value::from_bits(callee), ctx_raw, sp) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
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
    // The literal's own array is the fresh dense Array `array_begin`
    // created, so the index-native CreateDataProperty stores directly.
    let result = match array.kind() {
        ValueKind::Object(obj) => obj
            .create_data_property_index(index as u64, value)
            .map(|_| ()),
        _ => crate::ir::array_set(&array, &index.to_string(), value),
    };
    if let Err(error) = result {
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
        let result = match array.kind() {
            ValueKind::Object(obj) => obj
                .create_data_property_index(index as u64, value)
                .map(|_| ()),
            _ => crate::ir::array_set(&array, &index.to_string(), value),
        };
        if let Err(error) = result {
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
    // A dense literal whose element defines already reached the element
    // count (no trailing holes) needs no [[Set]] of length — assigning the
    // own length its current value is unobservable, and the full
    // ArraySetLength path costs ~1us per literal.
    let needs_set = match obj.array_length_dense() {
        Some(current) => current as usize != length,
        None => true,
    };
    if needs_set {
        match obj.set(
            &crux::JsString::from_utf8("length"),
            Value::Number(length as f64),
            true,
        ) {
            Ok(_) => {}
            Err(error) => return slow_error(ctx, error),
        }
    }
    array.bits()
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

extern "C" fn object_fast(ctx: *mut c_void, step: u64, sp: u64) -> u64 {
    // Cut 72: the compiled `Step::ObjectFast` whole-literal fused create.
    // The names payload is read back from the running body; the n values
    // sit below the machine-code working `sp` in source order (v0 lowest),
    // in the rooted JIT buffer, so the shared create (which allocates the
    // object and forks the shape chain) is safe to run under them. The
    // machine code drops the consumed values and pushes the returned
    // object.
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let Some(crate::ir::Step::ObjectFast { names }) = step_at(ctx, step) else {
        unreachable!("object_fast on a non-ObjectFast step");
    };
    let n = names.len();
    let base = (sp as usize).saturating_sub(n * 8);
    // SAFETY: `sp` points into the machine-code working region (the rooted
    // JIT buffer) with the n consumed values below it — the compiled
    // `ObjectFast` step pushed exactly one value per name in source order.
    let values: &[Value] = unsafe { std::slice::from_raw_parts(base as *const Value, n) };
    match crate::ir::object_fast_create(agent, names, values) {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
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
        span,
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
        *span,
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
    let Some(crate::ir::Step::ObjectAccessorComputed {
        get,
        param,
        body,
        span,
    }) = step_at(ctx, step)
    else {
        unreachable!("object_accessor_computed on a non-ObjectAccessorComputed step");
    };
    let strict = unsafe { &*ctx.body }.strict;
    let key = match crate::context::to_property_key(agent, &key) {
        Ok(key) => key,
        Err(error) => return slow_error(ctx, error),
    };
    match crate::ir::object_accessor(
        agent,
        &object,
        key,
        *get,
        param.as_ref(),
        body,
        strict,
        *span,
    ) {
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
    match crate::ir::concat_template(agent, &acc, &value) {
        Ok(out) => out.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

extern "C" fn concat_str_const(ctx: *mut c_void, acc: u64, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let acc = Value::from_bits(acc);
    let Some(crate::ir::Step::ConcatStrConst(text)) = step_at(ctx, step) else {
        unreachable!("concat_str_const on a non-ConcatStrConst step");
    };
    match crate::ir::concat_template_const(agent, &acc, text) {
        Ok(out) => out.bits(),
        Err(error) => slow_error(ctx, error),
    }
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
        (crate::ir::LeafOp::StoreMemberComputedSlot { key, .. }, 3) => reg_const(key),
        (crate::ir::LeafOp::StoreMemberComputedSlot { value, .. }, 4) => reg_const(value),
        (crate::ir::LeafOp::StoreMemberComputedLocal { key, .. }, 5) => reg_const(key),
        (crate::ir::LeafOp::CompoundMemberComputedLocal { rhs, .. }, 5) => reg_const(rhs),
        (crate::ir::LeafOp::CompoundMemberComputedLocal { key, .. }, 6) => reg_const(key),
        (crate::ir::LeafOp::UpdateMemberComputedLocal { key, .. }, 5) => reg_const(key),
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
/// Stage O: a compiled guard failed and the body bails to the interpreter
/// mid-run. The machine code set `vm.ip` to the step to resume at and
/// `suspend_sp` to the live working-region top; `run_jit_body` rebuilds
/// `vm.stack` from that region and returns `Interp`, so `run_compiled_body`
/// resumes the interpreter at `vm.ip`. A deopt is a resume, not an error:
/// the step is re-executed from scratch, so the guarded fast path must
/// leave the step's operands exactly as the interpreter expects them.
pub const DISPATCH_DEOPT: u64 = u64::MAX - 3;

/// A thrown value escaping the body becomes the same `JsError` the
/// interpreter's `body_completion_to_value` produces — the attached value
/// round-trips through the caller's `to_throwable`, so an enclosing catch
/// observes the original thrown value.
fn throw_value_error(value: Value) -> JsError {
    crate::flow::uncaught_error(value)
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

extern "C" fn exit_try(ctx: *mut c_void, ip: u64, after: u64, handler: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let body = unsafe { &*ctx.body };
    vm.ip = ip as usize;
    // The common `try`/`finally` exit: this handler's frame is the only one on
    // the stack, so the transfer leaves exactly it and the finally runs next —
    // `control_transfer` would scan the stack, find the same frame and take the
    // same arm, then route the result back through the dispatch chain.
    let handler = handler as usize;
    if vm.try_stack.len() == 1
        && vm.try_stack[0].handler == handler
        && let Some(finally) = body.handlers.get(handler).and_then(|h| h.finally)
    {
        let frame = vm.try_stack.remove(0);
        vm.enter_finally(
            frame,
            finally,
            &crate::ir::Ctl::Normal {
                after: after as usize,
            },
        );
        return finally as u64;
    }
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
    // The common case: the finally's try was the only frame and it is gone, so
    // re-applying a NORMAL control is the environment restore and the jump —
    // `control_transfer` would scan an empty stack and do exactly that.
    if vm.try_stack.is_empty()
        && let crate::ir::PendingControl::Normal {
            after, env, depth, ..
        } = pending
    {
        vm.restore_env(env, depth);
        vm.ip = after;
        return after as u64;
    }
    match pending {
        crate::ir::PendingControl::Normal {
            after, env, depth, ..
        } => {
            vm.restore_env(env, depth);
            let result = vm.control_transfer(agent, body, crate::ir::Ctl::Normal { after });
            dispatch_result(ctx, vm, result)
        }
        crate::ir::PendingControl::Break {
            target, env, depth, ..
        } => {
            vm.restore_env(env, depth);
            let result = vm.control_transfer(agent, body, crate::ir::Ctl::Break { target });
            dispatch_result(ctx, vm, result)
        }
        crate::ir::PendingControl::Continue {
            target, env, depth, ..
        } => {
            vm.restore_env(env, depth);
            let result = vm.control_transfer(agent, body, crate::ir::Ctl::Continue { target });
            dispatch_result(ctx, vm, result)
        }
        crate::ir::PendingControl::Return {
            value, env, depth, ..
        } => {
            vm.restore_env(env, depth);
            let result = vm.control_transfer(agent, body, crate::ir::Ctl::Return { value });
            dispatch_result(ctx, vm, result)
        }
        crate::ir::PendingControl::Throw {
            value, env, depth, ..
        } => {
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
    // catch body see it. A parameter the scan left UNBOUND (it shadows a live
    // same-name binding) is skipped: the slot belongs to the shadowed binding.
    if let Some(scope) = &body.scope
        && let Some(param) = param
        && let syntax::ast::BindingPattern::Ident(name) = param
        && !scope.shadowed_catch_params.contains(name)
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
    vm.switch_disc = Value::from_bits(value);
    vm.switch_disc_set = true;
    Value::Undefined.bits()
}

extern "C" fn switch_test(ctx: *mut c_void, case: u64, test: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let test = Value::from_bits(test);
    if !vm.switch_disc_set {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "SwitchTest without a discriminant".into(),
            ),
        );
    };
    if crux::ops::is_strictly_equal(&vm.switch_disc, &test) {
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
        vm.for_in_stack.push(crate::ir::ForInState {
            base: dummy,
            keys: Vec::new(),
            index: 0,
            base_generation: 0,
            fast: false,
        });
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
    let keys = if crate::ir::for_in_generation_tracked(&obj) {
        match crate::ir::for_in_cache_keys(agent, &obj) {
            Ok(Some(keys)) => keys,
            Ok(None) => {
                let keys = match crate::eval::for_in_key_levels(agent, &rhs) {
                    Ok(keys) => keys,
                    Err(error) => return slow_error(ctx, error),
                };
                if let Err(error) = crate::ir::for_in_cache_put(agent, &obj, &keys) {
                    return slow_error(ctx, error);
                }
                keys
            }
            Err(error) => return slow_error(ctx, error),
        }
    } else {
        match crate::eval::for_in_key_levels(agent, &rhs) {
            Ok(keys) => keys,
            Err(error) => return slow_error(ctx, error),
        }
    };
    let fast =
        crate::ir::for_in_generation_tracked(&obj) && keys.iter().all(|(level, _)| *level == 0);
    vm.for_in_stack.push(crate::ir::ForInState {
        base: obj,
        keys,
        index: 0,
        base_generation: obj.generation.get(),
        fast,
    });
    Value::Undefined.bits()
}

extern "C" fn for_in_next(ctx: *mut c_void, stack: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let vm = unsafe { &mut *ctx.vm };
    let Some(state) = vm.for_in_stack.last_mut() else {
        return slow_error(
            ctx,
            JsError::new(ErrorKind::SyntaxError, "ForInNext without a for-in".into()),
        );
    };
    if state.fast && state.base.generation.get() == state.base_generation {
        // The base is an ordinary/array object and has not structurally
        // changed since the keys were enumerated (any delete/define/attr flip
        // bumps the generation), so every remaining level-0 key is still an
        // own enumerable property — yield it without the per-key check.
        if state.index < state.keys.len() {
            let key = state.keys[state.index].1;
            state.index += 1;
            // SAFETY: the machine code passes its live working-stack
            // pointer with room for one slot.
            unsafe { *(stack as *mut u64) = key.bits() };
            return 1;
        }
        vm.for_in_stack.pop();
        return 0;
    }
    while state.index < state.keys.len() {
        let (_, key) = state.keys[state.index];
        state.index += 1;
        // A key deleted during enumeration is skipped (spec
        // EnumerateObjectProperties step 5.a.v).
        match crate::eval::for_in_key_still_visited(&state.base, &key) {
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
    let Some(crate::ir::Step::ForOfBegin { top, end, cursor }) = step_at(ctx, step) else {
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
    // The compiled fast cursor (G17): seed `(array_slot, index_slot)` with the
    // dense-Array Value (or `undefined` for every other receiver, so the
    // compiled advance takes the helper path) and index 0. `frame_get_mut` is
    // the same slot access the fused bind uses; a missing cursor leaves the
    // slots untouched. The index rides as a raw `u64` (never a `Value`: only
    // the compiled read touches it, and its tag bits are 0, so a frame scan
    // cannot mistake it for a heap pointer).
    let fast = match &entry {
        crate::ir::ForOfEntry::Fast { array, .. } => *array,
        crate::ir::ForOfEntry::Generic(_) => Value::Undefined,
    };
    vm.for_of_stack.push(entry);
    // The fixup-patched span drives `close_for_of_upto` on an external
    // break/return/throw (mirroring the interpreter handler).
    vm.for_of_boundaries.push((*top, *end));
    if cursor.0 != usize::MAX {
        *vm.frame_get_mut(cursor.0) = fast;
        // A raw element count, not a `Value`: only the compiled read touches
        // it (its tag bits stay 0, so a frame scan cannot read it as a heap
        // pointer), and it starts at 0.
        *vm.frame_get_mut(cursor.1) = Value::from_bits(0);
    }
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

/// The compiled fast-array cursor's fallback advance (G17): the inline
/// `ForOfNextBindLocal` advances its own frame-slot cursor without touching the
/// Vm entry, so the first step that cannot be served inline (a hole, the end, a
/// receiver that stopped being dense) must hand the entry the index the inline
/// path reached before running the shared `for_of_advance`. Only a `Fast`
/// entry carries an index; a `Generic` one ignores it.
extern "C" fn for_of_fast_next(ctx: *mut c_void, slot: u64, index: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    if let Some(crate::ir::ForOfEntry::Fast {
        index: entry_index, ..
    }) = vm.for_of_stack.last_mut()
    {
        *entry_index = index as usize;
    }
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

// ----- Cut 60: arguments objects -----

/// `Step::CreateArguments` (Cut 60): build the body's `arguments` object
/// from the call's arguments and store it into the frame slot. The sloppy
/// mapped form aliases the formal parameters through the capture context
/// (`vm.lexical_env` — the certified layout moved every param there) and
/// reads the running context's `function` for `callee`; the strict unmapped
/// form reads only the argument slice. Emitted once at body entry.
extern "C" fn create_arguments(ctx: *mut c_void, step: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let Some(crate::ir::Step::CreateArguments { slot, mapped }) = step_at(ctx, step) else {
        return slow_error(
            ctx,
            JsError::new(
                ErrorKind::SyntaxError,
                "CreateArguments without a slot".into(),
            ),
        );
    };
    let value = match mapped {
        Some(formals) => {
            let func = match agent.running_context() {
                Ok(context) => context.function.unwrap_or(Value::Undefined),
                Err(error) => return slow_error(ctx, error),
            };
            let formals: Vec<crux::JsString> = formals.iter().map(|id| crux::lookup(*id)).collect();
            match crate::function::create_mapped_arguments_object(
                agent,
                func,
                &vm.call_args,
                &formals,
                vm.lexical_env,
            ) {
                Ok(value) => value,
                Err(error) => return slow_error(ctx, error),
            }
        }
        None => match crate::function::create_unmapped_arguments_object(agent, &vm.call_args) {
            Ok(value) => value,
            Err(error) => return slow_error(ctx, error),
        },
    };
    *vm.frame_get_mut(*slot) = value;
    Value::Undefined.bits()
}

/// `Step::TypeofTop` (Cut 60): compute the `typeof` string of the popped
/// value (spec 13.5.3.2 — a value operand; the unresolvable-reference form
/// is `TypeofIdent`, which `typeof_ident` serves). Never errors.
extern "C" fn typeof_top(ctx: *mut c_void, value: u64) -> u64 {
    let _ = ctx;
    typeof_bits(&Value::from_bits(value))
}

/// The `typeof` string for `value`, as the value's bits — the shape
/// `typeof_top` (a value operand, Cut 60) and `typeof_ident` (a name resolved
/// through the environment) both push.
fn typeof_bits(value: &Value) -> u64 {
    Value::String(crux::Handle::new(crux::JsString::from_utf8(
        crux::value::type_of(value),
    )))
    .bits()
}

/// The typed-array length probe: the slots length of an IntegerIndexed
/// receiver (spec 25.2.3.2 — the value the `%TypedArray%.prototype.length`
/// accessor serves; the own-`length` shadowing is handled by the interpreter
/// side, so a probe hit here is exact), or the canonical-NaN sentinel for
/// any other receiver. The compiled `GetMemberName` with the `length` atom
/// compares against the sentinel and falls back to the member-cell probe /
/// `get_member_name` helper on a miss. Pure: `typed_array_effective_length`
/// reads only the slots + shared buffer (no user code, no Vm mutation), so
/// the call never sets the pending byte.
extern "C" fn typed_array_length(_ctx: *mut c_void, object: u64) -> u64 {
    let ValueKind::Object(obj) = Value::from_bits(object).kind() else {
        return TYPED_ARRAY_LENGTH_SENTINEL;
    };
    if let crux::object::ObjectKind::IntegerIndexed(slots) = &obj.kind
        // An own `length` data property shadows the %TypedArray%.prototype
        // accessor (spec OrdinaryGet — a define-created own property wins),
        // so the probe must miss and let the general read serve the own
        // value — the mirror of the interpreter's `get_member_name`
        // shortcut gate (a plain, usually empty own-property scan).
        && !obj.has_own_property_atom(length_atom())
    {
        return Value::Number(crux::object::typed_array_effective_length(slots) as f64).bits();
    }
    TYPED_ARRAY_LENGTH_SENTINEL
}

/// The interned "length" atom, cached once (the length probe runs every
/// iteration of a byte-copy loop, so it must not re-intern per read).
fn length_atom() -> crux::AtomId {
    use std::sync::OnceLock;
    static LENGTH: OnceLock<crux::AtomId> = OnceLock::new();
    *LENGTH.get_or_init(|| crux::string::intern_utf8("length"))
}

/// The probe's miss sentinel: the canonical quiet-NaN value (`Value` boxes a
/// NaN with the reserved-tag bits to this pattern; a length is never NaN, so
/// the compiled comparison is exact). Shared with the compiler via
/// `JitHelpers`-independent constants below.
pub const TYPED_ARRAY_LENGTH_SENTINEL: u64 = 0x7FF9_0000_0000_0000;

// ----- Cut 61: super property access -----

/// `Step::GetSuperBase` (Cut 61): the this-binding check, then the base —
/// the home object's [[Prototype]] for a certified body (the frame-slot
/// model creates no Function env), the env walk otherwise. Pushes the base.
extern "C" fn get_super_base(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm
        .vm_this_binding(agent)
        .and_then(|_| vm.vm_super_base(agent))
    {
        Ok(base) => base.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::ThisValue` (Cut 61): the current run's this binding — the
/// certified frame slot for a certified body, the Function env otherwise.
/// (`super.m()` calls push it as the receiver.)
extern "C" fn this_value(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm.vm_this_binding(agent) {
        Ok(this) => this.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::GetSuperName` (Cut 61): `base.name` with the this receiver
/// (spec 13.3.7.1). Returns the value bits.
extern "C" fn get_super_name(ctx: *mut c_void, base: u64, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let base = Value::from_bits(base);
    match vm.vm_this_binding(agent) {
        Ok(this) => {
            match crate::context::get_property(agent, &base, &crux::lookup(name as u32), this) {
                Ok(value) => value.bits(),
                Err(error) => slow_error(ctx, error),
            }
        }
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::GetSuperComputed` (Cut 61): convert the key, then `base[key]`
/// with the this receiver. Returns the value bits.
extern "C" fn get_super_computed(ctx: *mut c_void, base: u64, key: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let base = Value::from_bits(base);
    let key = Value::from_bits(key);
    match crate::context::to_property_key(agent, &key) {
        Ok(key) => match vm.vm_this_binding(agent) {
            Ok(this) => match crate::context::get_property_key(agent, &base, &key, this) {
                Ok(value) => value.bits(),
                Err(error) => slow_error(ctx, error),
            },
            Err(error) => slow_error(ctx, error),
        },
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::GetSuperComputedKeep` (Cut 61): like `get_super_computed`, but the
/// CONVERTED key is written at `stack[0]` (the machine code advances its sp
/// past it, then pushes the returned value — the write copy). The base was
/// captured by GetSuperBase, so a key whose toString mutates the prototype
/// still sees the original base (spec 13.3.7.1, mirroring
/// `GetMemberComputedKeep`).
extern "C" fn get_super_computed_keep(ctx: *mut c_void, stack: u64, base: u64, key: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let base = Value::from_bits(base);
    let key = Value::from_bits(key);
    let result = (|| -> Result<Value, JsError> {
        let key = crate::context::to_property_key(agent, &key)?;
        let this = vm.vm_this_binding(agent)?;
        let value = crate::context::get_property_key(agent, &base, &key, this)?;
        // The converted key's value form for the write (mirroring the
        // interpreter's pop + push of the converted key).
        let key_value = match key {
            crux::property::PropertyKey::String(id) => {
                Value::String(crux::Handle::new(crux::lookup(id)))
            }
            crux::property::PropertyKey::Symbol(symbol) => Value::Symbol(symbol),
        };
        // SAFETY: the machine code passed its live working-stack pointer
        // with the write-copy slot vacated.
        unsafe { *(stack as *mut u64) = key_value.bits() };
        Ok(value)
    })();
    match result {
        Ok(value) => value.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::AssignSuperName` (Cut 61): `super.x = v` / `super.x op= v` — the
/// base, the cached old value (compound ops), and the value are popped by
/// the machine code. Returns the assigned value (the assignment expression's
/// result).
extern "C" fn assign_super_name(
    ctx: *mut c_void,
    op: u64,
    base: u64,
    name: u64,
    old: u64,
    value: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let base = Value::from_bits(base);
    let value = Value::from_bits(value);
    let op = ASSIGN_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(AssignOp::Assign);
    let old = if crate::ir::is_compound_assign(&op) {
        Some(Value::from_bits(old))
    } else {
        None
    };
    match vm.assign_super_value(
        agent,
        base,
        PropertyKeyName::Name(name as u32),
        old,
        value,
        op,
    ) {
        Ok(result) => result.bits(),
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::AssignSuperComputed` (Cut 61): like `assign_super_name` with a
/// converted key (the machine code pops value, old, key, base).
extern "C" fn assign_super_computed(
    ctx: *mut c_void,
    op: u64,
    base: u64,
    key: u64,
    old: u64,
    value: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let base = Value::from_bits(base);
    let value = Value::from_bits(value);
    let key = Value::from_bits(key);
    let op = ASSIGN_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(AssignOp::Assign);
    let old = if crate::ir::is_compound_assign(&op) {
        Some(Value::from_bits(old))
    } else {
        None
    };
    match crate::context::to_property_key(agent, &key) {
        Ok(key) => {
            match vm.assign_super_value(agent, base, PropertyKeyName::Key(key), old, value, op) {
                Ok(result) => result.bits(),
                Err(error) => slow_error(ctx, error),
            }
        }
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::UpdateSuperName` (Cut 61): `super.x++`/`--` — pop the old value
/// and the base, compute the update, put it through the super reference, and
/// push the prefix/postfix result.
extern "C" fn update_super_name(
    ctx: *mut c_void,
    op: u64,
    prefix: u64,
    base: u64,
    name: u64,
    old: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let base = Value::from_bits(base);
    let old = Value::from_bits(old);
    let op = UPDATE_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(UpdateOp::Increment);
    match crate::ir::update_value(agent, &op, &old) {
        Ok((old_numeric, new)) => {
            match vm.put_super_value(agent, base, PropertyKeyName::Name(name as u32), new) {
                Ok(()) => (if prefix != 0 { new } else { old_numeric }).bits(),
                Err(error) => slow_error(ctx, error),
            }
        }
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::UpdateSuperComputed` (Cut 61): like `update_super_name` with a
/// converted key (the machine code pops old, key, base).
extern "C" fn update_super_computed(
    ctx: *mut c_void,
    op: u64,
    prefix: u64,
    base: u64,
    key: u64,
    old: u64,
) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let base = Value::from_bits(base);
    let key = Value::from_bits(key);
    let old = Value::from_bits(old);
    let op = UPDATE_OPS
        .get(op as usize)
        .copied()
        .unwrap_or(UpdateOp::Increment);
    match crate::context::to_property_key(agent, &key) {
        Ok(key) => match crate::ir::update_value(agent, &op, &old) {
            Ok((old_numeric, new)) => {
                match vm.put_super_value(agent, base, PropertyKeyName::Key(key), new) {
                    Ok(()) => (if prefix != 0 { new } else { old_numeric }).bits(),
                    Err(error) => slow_error(ctx, error),
                }
            }
            Err(error) => slow_error(ctx, error),
        },
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::DeleteSuper` (Cut 61): `delete super.x` — a ReferenceError before
/// the key is even evaluated (spec 13.5.1.2 step 4.b).
extern "C" fn delete_super(ctx: *mut c_void) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    slow_error(
        ctx,
        JsError::new(
            ErrorKind::ReferenceError,
            "Unsupported reference to 'super'".into(),
        ),
    )
}

/// `Step::ResolveSuperRefName` (Cut 61): build the super property reference
/// — the this binding, the base, the name — on the var_ref_stack (the
/// update/logical-assign paths share the reference helpers).
extern "C" fn resolve_super_ref_name(ctx: *mut c_void, name: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    match vm
        .vm_this_binding(agent)
        .and_then(|this| vm.vm_super_base(agent).map(|base| (this, base)))
    {
        Ok((this, base)) => {
            vm.var_ref_stack.push(crate::context::Reference {
                base: crate::context::ReferenceBase::Value(base),
                name: crux::property::PropertyKey::from_js_string(&crux::lookup(name as u32)),
                strict: vm.strict,
                this_value: Some(this),
                private_name: None,
            });
            Value::Undefined.bits()
        }
        Err(error) => slow_error(ctx, error),
    }
}

/// `Step::ResolveSuperRefComputed` (Cut 61): like `resolve_super_ref_name`
/// with a converted key (the machine code pops base + key).
extern "C" fn resolve_super_ref_computed(ctx: *mut c_void, base: u64, key: u64) -> u64 {
    let ctx = unsafe { ctx_of(ctx) };
    let agent = unsafe { &mut *ctx.agent };
    let vm = unsafe { &mut *ctx.vm };
    let base = Value::from_bits(base);
    let key = Value::from_bits(key);
    match crate::context::to_property_key(agent, &key) {
        Ok(key) => match vm.vm_this_binding(agent) {
            Ok(this) => {
                vm.var_ref_stack.push(crate::context::Reference {
                    base: crate::context::ReferenceBase::Value(base),
                    name: key,
                    strict: vm.strict,
                    this_value: Some(this),
                    private_name: None,
                });
                Value::Undefined.bits()
            }
            Err(error) => slow_error(ctx, error),
        },
        Err(error) => slow_error(ctx, error),
    }
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
        // Fresh env: one push instead of the create_mutable_binding
        // duplicate scan + initialize_binding re-find.
        env.push_initialized_binding(name, value)?;
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
            env.set_slot(index as usize, value);
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
            env.set_slot(index as usize, new);
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
            env.set_slot(index as usize, value);
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
            env.set_slot(index as usize, new);
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
        // The compile-size cap (Cut 92): a body this large is never compiled.
        // Compile cost is roughly linear in the step count, so a body above
        // the cap costs more to build than a pass over the workload repays;
        // the interpreter serves it, which is what the pre-`Unary` census did
        // for 111 of deno's biggest bodies anyway. The `1` mark is sticky
        // because the fact cannot change, and it is the same "known
        // non-compilable" state an unsupported step writes. An eviction
        // clears it, so a re-consult re-checks against the same constant.
        // A body with a self-tail-call is exempt: its iterations are unbounded
        // within ONE call (the step IS the interpreter's TCO loop, which
        // `body_has_loop` counts as a loop for the same reason), so the
        // compile amortizes inside a single call however large the body —
        // measured, the 65-argument vector self-jump of Cut 51/52 is well
        // above the cap and must compile (`installed_jit_runs_a_vector_self_
        // tail_call`).
        if ir.steps.len() > JIT_MAX_COMPILE_STEPS && !crate::ir::body_has_self_tail_call(&ir.steps)
        {
            // Observable through the same switch the emitter's refusals use
            // (`JIT_DUMP_CLIF`), so a census distinguishes "too large to
            // compile" from "a step the emitter cannot lower".
            if std::env::var("JIT_DUMP_CLIF").is_ok() {
                eprintln!("jit skip: body too large ({} steps)", ir.steps.len());
            }
            ir.jit_info.set(1);
            return std::ptr::null();
        }
        // Cut 69: a straight-line body (`has_loop` false) is run
        // interpreted until it has been consulted `JIT_COMPILE_THRESHOLD`
        // times. The count is deliberately NOT cached (`jit_info` stays 0),
        // so every consult re-counts and promotion is never blocked; the
        // `1` sticky-unsupported mark is never written here, so a genuinely
        // unsupported body fails once (at or past the threshold) and
        // sticks. A loop body compiles on the first consult — it runs once
        // with many internal iterations, so a pure count would never
        // promote it. `saturating_add` bounds the counter.
        //
        // An EVICTED body waits for the threshold again whatever its shape:
        // its slot was lost to a program whose working set exceeds the cache,
        // and without this a body used once per frame is recompiled on every
        // frame — a small body compiles in ~0.4ms, which is a frame's budget.
        // The cache bumps `jit_evictions` and resets the count when it evicts.
        if ir.jit_calls.get() < JIT_COMPILE_THRESHOLD
            && (!ir.has_loop || ir.jit_evictions.get() > 0)
        {
            ir.jit_calls.set(ir.jit_calls.get().saturating_add(1));
            return std::ptr::null();
        }
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
/// M10: the current realm's %Function.prototype.apply%/%call% intrinsic
/// bits for the compiled `CallApply` fast path's identity check. `(0, 0)`
/// when no realm is current or the intrinsic is not yet installed — the
/// machine code's check then never matches and the site falls back to the
/// `call_apply` slow path (always correct). The intrinsic functions are
/// installed once per realm at bootstrap and never reassigned, so the bits
/// stay valid for the whole run.
pub(crate) fn call_apply_intrinsic_bits(agent: &Agent) -> (u64, u64) {
    let Ok(realm) = agent.current_realm() else {
        return (0, 0);
    };
    (
        realm
            .intrinsics
            .apply_builtin()
            .map(|value| value.bits())
            .unwrap_or(0),
        realm
            .intrinsics
            .call_builtin()
            .map(|value| value.bits())
            .unwrap_or(0),
    )
}

/// The realm's identity bits for every recognized Stage-B intrinsic, indexed by
/// `Intrinsic as usize` (all 0 when no realm is current — the identity checks
/// then never match and the sites fall back to the general call).
pub(crate) fn intrinsic_bits(agent: &Agent) -> [u64; INTRINSIC_COUNT] {
    let Ok(realm) = agent.current_realm() else {
        return [0; INTRINSIC_COUNT];
    };
    let mut bits = [0u64; INTRINSIC_COUNT];
    for (slot, kind) in bits.iter_mut().zip(INTRINSICS) {
        *slot = realm
            .intrinsics
            .math(kind)
            .map(|value| value.bits())
            .unwrap_or(0);
    }
    bits
}

pub(crate) fn run_jit_body(
    agent: &mut Agent,
    vm: &mut Vm,
    ir: &std::rc::Rc<CompiledBody>,
    self_call_ok: bool,
) -> Result<JitRunOutcome, JsError> {
    init_deopt_probe_env();
    let Some(hook) = agent.jit_hook else {
        return Ok(JitRunOutcome::Interp);
    };
    // The recursion guard (see `Vm::run_jit_leaf`): beyond the cap, fall
    // back to the interpreter so the JIT's private working buffers cannot
    // exhaust the native stack.
    if agent.jit_depth >= MAX_JIT_DEPTH {
        return Ok(JitRunOutcome::Interp);
    }
    // G15: a top-level run may start after a compile evicted code, freeing the
    // machine entries the agent's shared leaf-call record table still named
    // (records are keyed by callee identity, and the entry they hold would
    // dangle). Bump the code generation so this run re-probes rather than
    // jumping to a freed entry. A nested run shares the enclosing run's
    // generation: no eviction can happen while a frame is in flight.
    if agent.jit_depth == 0 {
        agent.leaf_gen = agent.leaf_gen.wrapping_add(1);
    }
    // The JIT side of the JS stack-exhaustion guard (the interpreter checks
    // in `run_inner`): refuse to start a compiled activation that would
    // descend into the reserved bottom margin, throwing a catchable
    // RangeError instead of overflowing the native stack.
    crate::stack::enter_js(agent)?;
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
    debug_assert!(
        vm.nested_frame.is_none(),
        "run_jit_body addresses `vm.frame` directly, so a nested_frame pointer would disagree with the frame the entry is handed"
    );
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
    // A compiled `LoadIdent` hit serves the global-value cell for the name, and
    // that cell table is shared BY NAME across bodies — so the body's own reads
    // must be unshadowed, not merely "the chain is the global env" (a nested
    // body's wrapper could bind a name another body warmed the cell for).
    // `Vm::global_reads_are_unshadowed` walks the chain once for exactly the
    // names this body reads; a certified body adds no envs mid-run (no
    // `with`/`eval` in its own statements), so one walk at entry covers the run.
    let globals_unshadowed = {
        let current = agent.running_context()?.lexical_environment;
        Vm::global_reads_are_unshadowed(current, &ir.ident_names)
    };
    // M10: the compiled `CallApply` fast path compares the member-read
    // result against the realm's intrinsic — snapshot the bits per run (a
    // body with no apply site skips the realm read entirely).
    let (apply_builtin_bits, call_builtin_bits) = if ir.has_call_apply {
        call_apply_intrinsic_bits(agent)
    } else {
        (0, 0)
    };
    let intrinsic_bits = if ir.has_call_intrinsic {
        intrinsic_bits(agent)
    } else {
        [0; INTRINSIC_COUNT]
    };
    let mut ctx = JitCallContext {
        pending: false,
        error: None,
        agent: agent as *mut Agent,
        vm: vm as *mut Vm,
        global_object: global.as_ptr() as *mut c_void,
        global_value_cells: agent.global_value_cells.as_ptr() as *mut c_void,
        member_value_cells: agent.member_value_cells.as_ptr() as *mut c_void,
        computed_read_cells: agent.computed_read_cells.as_ptr() as *mut c_void,
        member_map_cells: agent.member_map_cells.as_ptr() as *mut c_void,
        globals_unshadowed,
        buf_end: (work_ptr as usize + work_len * std::mem::size_of::<Value>()) as *mut c_void,
        leaf_epoch: 0,
        leaf_records: agent.leaf_records.as_mut_ptr(),
        leaf_gen: agent.leaf_gen,
        body: std::rc::Rc::as_ptr(ir),
        tail: false,
        current_function: vm.current_function.map(|value| value.bits()).unwrap_or(0),
        self_entry: if self_call_ok { info.entry as u64 } else { 0 },
        self_stack_usage: info.stack_usage as u64,
        self_inline_ok: self_call_ok,
        apply_builtin_bits,
        call_builtin_bits,
        intrinsic_bits,
        dispatch_value: 0,
        suspension: None,
        suspend_sp: 0,
        global_bits: Value::Object(global).bits(),
        resume_kind: 0,
        resume_ip: 0,
        resume_sp: 0,
        resume_value: 0,
        gc_ticks: JIT_GC_PROBE_INTERVAL,
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
    if result == DISPATCH_DEOPT {
        // Stage O: a compiled guard failed. The machine code already set
        // `vm.ip` to the step to resume at and `suspend_sp` to the live
        // working-region top. The activation's Vm is fresh, so the working
        // region starts at `vm.stack[0]`; rebuilding the operand stack from
        // it leaves the interpreter exactly where the guard was, and it
        // re-executes that step from scratch.
        let depth = (ctx.suspend_sp as usize - work_ptr as usize) / std::mem::size_of::<Value>();
        vm.stack.clear();
        vm.stack.extend_from_slice(&work[..depth]);
        JIT_DEOPT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if JIT_DEOPT_TRACE.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("jit-deopt step={} depth={}", vm.ip, depth);
        }
        return Ok(JitRunOutcome::Interp);
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
        match run_jit_body(agent, vm, ir, false)? {
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
    // G15: see `run_jit_body` — a resumed body is a fresh run, and its lookup
    // below may evict code a recorded entry named.
    if agent.jit_depth == 0 {
        agent.leaf_gen = agent.leaf_gen.wrapping_add(1);
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
    // See `run_jit_body`: a `LoadIdent` hit serves the global-value cell, whose
    // table is shared by name across bodies, so the body's own reads must be
    // unshadowed (walked once here, for the names this body reads).
    let globals_unshadowed = {
        let current = agent.running_context()?.lexical_environment;
        Vm::global_reads_are_unshadowed(current, &ir.ident_names)
    };
    // M10: see `run_jit_body` — the resumed body's `CallApply` sites compare
    // against the realm's intrinsic bits.
    let (apply_builtin_bits, call_builtin_bits) = if ir.has_call_apply {
        call_apply_intrinsic_bits(agent)
    } else {
        (0, 0)
    };
    let intrinsic_bits = if ir.has_call_intrinsic {
        intrinsic_bits(agent)
    } else {
        [0; INTRINSIC_COUNT]
    };
    let mut ctx = JitCallContext {
        pending: false,
        error: None,
        agent: agent as *mut Agent,
        vm: vm as *mut Vm,
        global_object: global.as_ptr() as *mut c_void,
        global_value_cells: agent.global_value_cells.as_ptr() as *mut c_void,
        member_value_cells: agent.member_value_cells.as_ptr() as *mut c_void,
        computed_read_cells: agent.computed_read_cells.as_ptr() as *mut c_void,
        member_map_cells: agent.member_map_cells.as_ptr() as *mut c_void,
        globals_unshadowed,
        buf_end: (work_ptr as usize + work_len * std::mem::size_of::<Value>()) as *mut c_void,
        leaf_epoch: 0,
        leaf_records: agent.leaf_records.as_mut_ptr(),
        leaf_gen: agent.leaf_gen,
        body: std::rc::Rc::as_ptr(ir),
        tail: false,
        current_function: vm.current_function.map(|value| value.bits()).unwrap_or(0),
        self_entry: 0,
        self_stack_usage: 0,
        self_inline_ok: false,
        apply_builtin_bits,
        call_builtin_bits,
        intrinsic_bits,
        dispatch_value: 0,
        suspension: None,
        suspend_sp: 0,
        global_bits: Value::Object(global).bits(),
        resume_kind: kind,
        resume_ip: vm.ip,
        resume_sp: (work_ptr as usize + sp_offset * std::mem::size_of::<Value>()) as u64,
        resume_value: resume_value.bits(),
        gc_ticks: JIT_GC_PROBE_INTERVAL,
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
    use crux::handle::Handle;
    use crux::string::JsString;

    /// The compiled `EnterTry`/`Exit` fast paths read a `Vec`'s cursor fields
    /// (`VM_TRY_STACK_PTR_OFFSET`/`CAP_OFFSET`/`LEN_OFFSET`) from machine code.
    /// Those are private fields of `Vec`, so nothing else checks these offsets:
    /// a toolchain that lays `Vec` out differently must fail here rather than
    /// let a compiled body write a `TryFrame` through a misread pointer.
    #[test]
    fn vec_cursor_offsets_match_the_compiled_paths() {
        let mut frames: Vec<crate::ir::TryFrame> = Vec::with_capacity(7);
        let words = unsafe {
            std::slice::from_raw_parts(
                &frames as *const Vec<crate::ir::TryFrame> as *const usize,
                3,
            )
        };
        assert_eq!(
            words[VEC_CAP_OFFSET / std::mem::size_of::<usize>()],
            7,
            "capacity"
        );
        assert_eq!(
            words[VEC_PTR_OFFSET / std::mem::size_of::<usize>()],
            frames.as_ptr() as usize,
            "data pointer"
        );
        assert_eq!(
            words[VEC_LEN_OFFSET / std::mem::size_of::<usize>()],
            0,
            "length"
        );
        frames.push(crate::ir::TryFrame {
            handler: 3,
            saved_env: Handle::new(crate::env::EnvRecord::Declarative(
                crate::env::DeclarativeEnv::new(None),
            )),
            env_depth: 1,
        });
        let words = unsafe {
            std::slice::from_raw_parts(
                &frames as *const Vec<crate::ir::TryFrame> as *const usize,
                3,
            )
        };
        assert_eq!(
            words[VEC_LEN_OFFSET / std::mem::size_of::<usize>()],
            1,
            "length after push"
        );
    }

    fn scope_info(frame_size: usize) -> crate::ir::ScopeInfo {
        crate::ir::ScopeInfo {
            frame_size,
            arity: 0,
            slots: Default::default(),
            tdz_store: vec![false; frame_size],
            shadow_slots: Default::default(),
            shadowed_catch_params: Default::default(),
            context_names: Vec::new(),
            context_tdz: Vec::new(),
            context_const: Vec::new(),
            context_param: Vec::new(),
            context_slots: Default::default(),
            arguments_slot: None,
            arguments_formals: None,
            this_slot: None,
            captured_this: None,
            args_alias: false,
            annex_b: Vec::new(),
            statement_fns: Vec::new(),
        }
    }

    /// G14: the record's fill descriptor must describe the frame the probe
    /// actually fills — the `this` slot, strictness and the per-slot TDZ bits —
    /// and must clear `fill_ok` for a frame wider than the mask, where the
    /// compiled hit path falls back to the probe instead of mis-filling.
    #[test]
    fn leaf_fill_descriptor_matches_the_scope() {
        let mut scope = scope_info(3);
        scope.arity = 1;
        scope.tdz_store = vec![false, true, false];
        scope.this_slot = Some(2);
        let info = leaf_fill_info(&scope, false, 0x1234, 7, false);
        assert_eq!(info.entry, 0x1234);
        assert_eq!(info.stack_usage, 7);
        assert_eq!(info.frame_size, 3);
        assert_eq!(info.arity, 1);
        assert_eq!(info.this_slot, 2);
        assert_eq!(info.strict, 0);
        assert_eq!(info.tdz_mask, 1 << 1, "only slot 1 is lexical");
        assert_eq!(info.fill_ok, 1);
        assert_eq!(info.uses_env, 0);

        assert_eq!(leaf_fill_info(&scope, true, 0, 0, true).strict, 1);
        assert_eq!(
            leaf_fill_info(&scope, false, 0, 0, true).uses_env,
            1,
            "an env leaf must be recorded as such"
        );
        scope.this_slot = None;
        assert_eq!(
            leaf_fill_info(&scope, false, 0, 0, false).this_slot,
            NO_THIS_SLOT
        );

        let wide = scope_info(TDZ_MASK_SLOTS + 1);
        let info = leaf_fill_info(&wide, false, 0, 0, false);
        assert_eq!(
            info.fill_ok, 0,
            "a frame wider than the mask must fall back"
        );
        assert_eq!(info.tdz_mask, 0);
    }

    #[test]
    fn the_slow_path_table_is_complete() {
        // Every helper is a real function pointer (the JIT bails on a None
        // helper, so a null here would silently drop bodies to the
        // interpreter).
        assert_ne!(JIT_SLOW_PATHS.binary_slow as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.unary_slow as usize, 0);
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
        assert_ne!(JIT_SLOW_PATHS.typeof_ident as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.resolve_var_ident as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.put_var_reference as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.update_ident as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.assign_member_name as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.assign_member_computed as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.fast_array_element_write as usize, 0);
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
        assert_ne!(JIT_SLOW_PATHS.construct as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.tail_call_vector as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.tail_call_self_vector as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.array_begin as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.array_element as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.array_spread as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.array_hole as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.array_end as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.object_begin as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.object_fast as usize, 0);
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
        assert_ne!(JIT_SLOW_PATHS.create_arguments as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.typeof_top as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.typed_array_length as usize, 0);
        assert_ne!(JIT_SLOW_PATHS.get_super_base as usize, 0);
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
        assert_eq!(UNARY_OPS.len(), 7);
        assert_eq!(UNARY_OPS[UnaryOp::Plus as usize], UnaryOp::Plus);
        assert_eq!(UNARY_OPS[UnaryOp::BitNot as usize], UnaryOp::BitNot);
        assert_eq!(UNARY_OPS[UnaryOp::Not as usize], UnaryOp::Not);
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

    #[test]
    fn fast_array_element_write_stores_typed_array_elements() {
        // The JIT's inline store helper handles IntegerIndexed receivers
        // (previously only plain Arrays): a canonical Number index stores
        // through `typed_array_element_set`. Only primitive values are
        // accepted — an Object/Function value would run user code in the
        // coercion, which the helper must not do (0 falls back to the full
        // `assign_member_computed` helper that runs it on the VM).
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let ctx = std::ptr::null_mut();

        let number_array = agent.run_script("new Uint8Array(4)").unwrap();
        assert_eq!(
            fast_array_element_write(
                ctx,
                number_array.bits(),
                Value::Number(1.0).bits(),
                Value::Number(42.0).bits()
            ),
            1
        );
        let ValueKind::Object(number_obj) = number_array.kind() else {
            panic!("typed array is not an object");
        };
        let crux::object::ObjectKind::IntegerIndexed(slots) = &number_obj.kind else {
            panic!("not an IntegerIndexed object");
        };
        assert_eq!(
            number_obj.typed_array_element_get(slots, 1).unwrap(),
            Value::Number(42.0)
        );
        // A string value coerces through ToNumber without user code.
        assert_eq!(
            fast_array_element_write(
                ctx,
                number_array.bits(),
                Value::Number(2.0).bits(),
                Value::String(Handle::new(JsString::from_utf8("7"))).bits()
            ),
            1
        );
        assert_eq!(
            number_obj.typed_array_element_get(slots, 2).unwrap(),
            Value::Number(7.0)
        );
        // A BigInt on a Number array and an Object value fall back (the
        // fallback re-runs the coercion and throws/coerces on the VM).
        assert_eq!(
            fast_array_element_write(
                ctx,
                number_array.bits(),
                Value::Number(3.0).bits(),
                Value::BigInt(Handle::new(crux::BigInt::from(1u64))).bits()
            ),
            0
        );
        let object_value = agent.run_script("({})").unwrap();
        assert_eq!(
            fast_array_element_write(
                ctx,
                number_array.bits(),
                Value::Number(0.0).bits(),
                object_value.bits()
            ),
            0
        );
        // A BigInt array stores a BigInt value; a Number value falls back.
        let bigint_array = agent.run_script("new BigInt64Array(2)").unwrap();
        assert_eq!(
            fast_array_element_write(
                ctx,
                bigint_array.bits(),
                Value::Number(0.0).bits(),
                Value::BigInt(Handle::new(crux::BigInt::from(5u64))).bits()
            ),
            1
        );
        assert_eq!(
            fast_array_element_write(
                ctx,
                bigint_array.bits(),
                Value::Number(1.0).bits(),
                Value::Number(3.0).bits()
            ),
            0
        );
        // An out-of-bounds canonical index is a spec no-op that reports
        // success (10.4.7.6), matching `assign_computed_plain`.
        assert_eq!(
            fast_array_element_write(
                ctx,
                number_array.bits(),
                Value::Number(99.0).bits(),
                Value::Number(1.0).bits()
            ),
            1
        );
        // A non-integer / negative / non-Number key and a non-object
        // receiver still fall back.
        assert_eq!(
            fast_array_element_write(
                ctx,
                number_array.bits(),
                Value::Number(1.5).bits(),
                Value::Number(1.0).bits()
            ),
            0
        );
        assert_eq!(
            fast_array_element_write(
                ctx,
                number_array.bits(),
                Value::Number(-1.0).bits(),
                Value::Number(1.0).bits()
            ),
            0
        );
        assert_eq!(
            fast_array_element_write(
                ctx,
                Value::Number(1.0).bits(),
                Value::Number(0.0).bits(),
                Value::Number(1.0).bits()
            ),
            0
        );
    }

    #[test]
    fn typed_array_length_probe_serves_slots_or_the_sentinel() {
        // The compiled `GetMemberName`-with-length fast path: the helper
        // returns the slots length for an IntegerIndexed receiver and the
        // canonical-NaN sentinel for everything else (a length is never NaN,
        // so the machine code's equality test is exact). Pure — no ctx.
        let ctx = std::ptr::null_mut();
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();

        let array = agent.run_script("new Uint8Array(7)").unwrap();
        assert_eq!(
            typed_array_length(ctx, array.bits()),
            Value::Number(7.0).bits()
        );
        // A subarray's length is its element count, not the buffer's.
        let sub = agent
            .run_script("new Uint8Array(new ArrayBuffer(16), 4, 3)")
            .unwrap();
        assert_eq!(
            typed_array_length(ctx, sub.bits()),
            Value::Number(3.0).bits()
        );
        // A detached view reads 0 (spec 25.2.3.1), matching the accessor.
        assert_eq!(
            typed_array_length(
                ctx,
                agent
                    .run_script("(function(){ var b = new ArrayBuffer(4); var t = new Uint8Array(b); b.transfer(); return t; })()")
                    .unwrap()
                    .bits()
            ),
            Value::Number(0.0).bits()
        );
        // Non-typed-array receivers miss to the sentinel (the fallback
        // `get_member_name` serves ordinary objects and strings).
        assert_eq!(
            typed_array_length(ctx, agent.run_script("({ length: 3 })").unwrap().bits()),
            TYPED_ARRAY_LENGTH_SENTINEL
        );
        assert_eq!(
            typed_array_length(
                ctx,
                Value::String(Handle::new(JsString::from_utf8("abc"))).bits()
            ),
            TYPED_ARRAY_LENGTH_SENTINEL
        );
        assert_eq!(
            typed_array_length(ctx, Value::Undefined.bits()),
            TYPED_ARRAY_LENGTH_SENTINEL
        );
    }

    /// A non-null info pointer (> 1 as usize — 1 is the sticky-unsupported
    /// sentinel); never dereferenced by these tests.
    const FAKE_INFO: usize = 0x1000;

    /// Separate counters per test: the tests run in parallel threads, so a
    /// shared static would leak one test's hook calls into the other's
    /// assertions.
    static FAKE_LOOKUP_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static FAKE_LOOP_LOOKUP_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static FAKE_CAP_LOOKUP_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static FAKE_EVICT_LOOKUP_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// A fake cache lookup that always "compiles" (returns `FAKE_INFO`).
    unsafe extern "C" fn fake_lookup(
        _cache: *mut c_void,
        _body: *const c_void,
        _in_flight: bool,
    ) -> *const c_void {
        FAKE_LOOKUP_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        FAKE_INFO as *const c_void
    }

    unsafe extern "C" fn fake_loop_lookup(
        _cache: *mut c_void,
        _body: *const c_void,
        _in_flight: bool,
    ) -> *const c_void {
        FAKE_LOOP_LOOKUP_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        FAKE_INFO as *const c_void
    }

    unsafe extern "C" fn fake_cap_lookup(
        _cache: *mut c_void,
        _body: *const c_void,
        _in_flight: bool,
    ) -> *const c_void {
        FAKE_CAP_LOOKUP_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        FAKE_INFO as *const c_void
    }

    unsafe extern "C" fn fake_evict_lookup(
        _cache: *mut c_void,
        _body: *const c_void,
        _in_flight: bool,
    ) -> *const c_void {
        FAKE_EVICT_LOOKUP_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        FAKE_INFO as *const c_void
    }

    unsafe extern "C" fn fake_drop_cache(_cache: *mut c_void) {}

    fn fake_lookup_calls() -> usize {
        FAKE_LOOKUP_CALLS.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn fake_cap_lookup_calls() -> usize {
        FAKE_CAP_LOOKUP_CALLS.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn fake_evict_lookup_calls() -> usize {
        FAKE_EVICT_LOOKUP_CALLS.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn make_body(steps: Vec<crate::ir::Step>, has_loop: bool) -> std::rc::Rc<CompiledBody> {
        std::rc::Rc::new(CompiledBody {
            steps,
            handlers: Vec::new(),
            strict: false,
            scope: None,
            env_constant: true,
            leaf: false,
            leaf_needs_env: false,
            leaf_uses_env: false,
            leaf_ops: None,
            script_globals: None,
            jit_info: std::cell::Cell::new(0),
            jit_calls: std::cell::Cell::new(0),
            jit_evictions: std::cell::Cell::new(0),
            ident_names: Vec::new(),
            has_loop,
            has_call_apply: false,
            has_call_intrinsic: false,
        })
    }

    #[test]
    fn lookup_info_gates_straight_line_bodies_below_the_threshold() {
        // Cut 69: a straight-line body (`has_loop` false) is run
        // interpreted until its consult count reaches the threshold —
        // `lookup_info` returns null without consulting the hook, and the
        // count is NOT cached (`jit_info` stays 0), so every consult
        // re-counts. The (K+1)th consult compiles, and the set pointer then
        // skips the hook.
        FAKE_LOOKUP_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
        let hook = JitHook {
            cache: std::ptr::null_mut(),
            lookup: fake_lookup,
            drop_cache: fake_drop_cache,
            helpers: &JIT_SLOW_PATHS as *const JitSlowPaths,
        };
        let body = make_body(vec![crate::ir::Step::Push(Value::Undefined)], false);
        for _ in 0..JIT_COMPILE_THRESHOLD {
            assert!(lookup_info(hook, &body, false).is_null());
        }
        assert_eq!(body.jit_calls.get(), JIT_COMPILE_THRESHOLD);
        assert_eq!(
            body.jit_info.get(),
            0,
            "below-threshold misses are not cached"
        );
        assert_eq!(
            fake_lookup_calls(),
            0,
            "the hook is not consulted below the threshold"
        );
        let ptr = lookup_info(hook, &body, false);
        assert!(!ptr.is_null());
        assert_eq!(fake_lookup_calls(), 1);
        assert_eq!(body.jit_info.get(), FAKE_INFO);
        // A set pointer is returned directly; the hook is not re-consulted.
        let ptr2 = lookup_info(hook, &body, false);
        assert_eq!(ptr, ptr2);
        assert_eq!(fake_lookup_calls(), 1);
    }

    #[test]
    fn lookup_info_compiles_loop_bodies_on_the_first_consult() {
        // Cut 69: a loop body (`has_loop` true) bypasses the threshold — it
        // runs once with many internal iterations, so a pure count would
        // never promote it.
        FAKE_LOOP_LOOKUP_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
        let hook = JitHook {
            cache: std::ptr::null_mut(),
            lookup: fake_loop_lookup,
            drop_cache: fake_drop_cache,
            helpers: &JIT_SLOW_PATHS as *const JitSlowPaths,
        };
        let body = make_body(vec![crate::ir::Step::Push(Value::Undefined)], true);
        assert!(!lookup_info(hook, &body, false).is_null());
        assert_eq!(
            FAKE_LOOP_LOOKUP_CALLS.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a loop body compiles on first use"
        );
        assert_eq!(
            body.jit_calls.get(),
            0,
            "the threshold count is skipped for loops"
        );
    }

    #[test]
    fn a_declarative_global_read_warms_the_value_cell() {
        // A top-level `let`/`const` lives in the global env's DECLARATIVE
        // record, so no property slot describes it — but it is cacheable: the
        // cell records the value load-only, and the global env bumps the global
        // object's generation on every declarative mutation, which is exactly
        // what the cell's validation reads. The interpreter warms it here; the
        // compiled `LoadIdent` probe reads the same agent table.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let name = crux::string::intern_utf8("K");
        let index = name as usize & (crate::ir::GLOBAL_CELLS - 1);
        agent
            .run_script("let K = 3; function read() { return K; } read();")
            .unwrap();
        let warmed = agent.global_value_cells[index];
        assert_eq!(warmed.name, name, "the declarative read warmed the cell");
        assert_eq!(warmed.value.as_number(), Some(3.0));
        assert_eq!(
            warmed.slot,
            u32::MAX,
            "a declarative cell has no property slot to write through"
        );
        // A write to the binding must move the generation the cell captured, or
        // a compiled read would keep serving the pre-write value.
        agent.run_script("K = 4; read();").unwrap();
        let refreshed = agent.global_value_cells[index];
        assert_eq!(
            refreshed.value.as_number(),
            Some(4.0),
            "the post-write read re-warmed the cell"
        );
        assert_ne!(
            refreshed.generation, warmed.generation,
            "the declarative write bumped the generation"
        );
    }

    #[test]
    fn global_reads_are_unshadowed_checks_each_name_against_the_chain() {
        use crate::env::{
            new_declarative_environment, new_global_environment, new_object_environment,
        };
        let object = crux::object::JsObject::ordinary_object_create(None);
        let global = new_global_environment(object, object);
        let x = crux::string::intern_utf8("x");
        let y = crux::string::intern_utf8("y");
        // The bare global record cannot shadow a read, and a body that reads no
        // global through the chain has nothing to admit.
        assert!(Vm::global_reads_are_unshadowed(global, &[x]));
        assert!(!Vm::global_reads_are_unshadowed(global, &[]));
        // A wrapper binding the name blocks THAT name's read, not another's.
        // (A declared enclosing binding is compiled as a CAPTURE — a context
        // slot — rather than a `LoadIdent`, so this shape does not actually
        // reach the probe; the walk is exercised here for the mechanism, and
        // the reachable shadow is a DYNAMIC one: an eval-injected `var`, or a
        // `with` object's property.)
        let wrapper = new_declarative_environment(Some(global));
        wrapper
            .create_mutable_binding(&crux::lookup(x), false)
            .unwrap();
        wrapper
            .initialize_binding(&crux::lookup(x), Value::Number(2.0))
            .unwrap();
        assert!(!Vm::global_reads_are_unshadowed(wrapper, &[x]));
        assert!(Vm::global_reads_are_unshadowed(wrapper, &[y]));
        assert!(!Vm::global_reads_are_unshadowed(wrapper, &[x, y]));
        // A `with` scope is refused even when it does not bind the name: its
        // object can gain the property later without moving the global's
        // generation, which is all the cell's validation reads.
        let with_object = crux::object::JsObject::ordinary_object_create(None);
        let with_env = new_object_environment(with_object, true, Some(wrapper));
        assert!(!Vm::global_reads_are_unshadowed(with_env, &[y]));
        // A chain that never reaches a global record is refused too.
        let detached = new_declarative_environment(None);
        assert!(!Vm::global_reads_are_unshadowed(detached, &[y]));
    }

    #[test]
    fn a_nested_global_read_warms_the_value_cell() {
        // A helper nested in another function is not on the bare global record,
        // but its chain cannot shadow the name it reads as a global — so the
        // gate has to admit it and the read has to warm the cell (it used to be
        // refused for every nested body, at a resolve per read).
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let name = crux::string::intern_utf8("Math");
        let index = name as usize & (crate::ir::GLOBAL_CELLS - 1);
        let value = agent
            .run_script(
                "function outer() { \
                   function inner() { var s = 0; \
                     for (var i = 0; i < 4; i++) { s += Math.PI; } \
                     return s; } \
                   return inner(); } \
                 outer();",
            )
            .unwrap();
        assert!(value.as_number().is_some(), "the nested loop ran");
        let cell = agent.global_value_cells[index];
        assert_eq!(
            cell.name, name,
            "the nested read warmed the global-value cell"
        );
        assert_ne!(
            cell.slot,
            u32::MAX,
            "`Math` is an object-record binding, so the cell carries its slot"
        );
    }

    #[test]
    fn lookup_info_refuses_bodies_above_the_compile_size_cap() {
        // Cut 92: a body above the size cap is never compiled, whatever its
        // shape — a compile costs roughly linear in the step count, so a body
        // this large is not repaid by a pass over the workload. The refusal is
        // sticky (the same "known non-compilable" state an unsupported step
        // writes), so the hook is never consulted and the threshold count is
        // not spent.
        FAKE_CAP_LOOKUP_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
        let hook = JitHook {
            cache: std::ptr::null_mut(),
            lookup: fake_cap_lookup,
            drop_cache: fake_drop_cache,
            helpers: &JIT_SLOW_PATHS as *const JitSlowPaths,
        };
        let over = make_body(
            vec![crate::ir::Step::Push(Value::Undefined); JIT_MAX_COMPILE_STEPS + 1],
            true,
        );
        assert!(lookup_info(hook, &over, false).is_null());
        assert_eq!(over.jit_info.get(), 1, "the refusal is sticky");
        assert_eq!(over.jit_calls.get(), 0, "the threshold count is not spent");
        assert_eq!(
            fake_cap_lookup_calls(),
            0,
            "an oversized body never reaches the hook"
        );
        // A repeated consult stays refused through the sticky mark.
        assert!(lookup_info(hook, &over, false).is_null());
        assert_eq!(fake_cap_lookup_calls(), 0);
        // A body exactly at the cap compiles as usual.
        let at_cap = make_body(
            vec![crate::ir::Step::Push(Value::Undefined); JIT_MAX_COMPILE_STEPS],
            true,
        );
        assert!(!lookup_info(hook, &at_cap, false).is_null());
        assert_eq!(fake_cap_lookup_calls(), 1);
        // A body with a self-tail-call is exempt whatever its size: its
        // per-call iterations are unbounded, so its compile always amortizes
        // inside one call. Without the exemption the 65-argument vector
        // self-jump (`installed_jit_runs_a_vector_self_tail_call`) lands on
        // the interpreter and the compiled-chain contract is lost.
        let mut oversized_tail_call =
            vec![crate::ir::Step::Push(Value::Undefined); JIT_MAX_COMPILE_STEPS + 1];
        oversized_tail_call.push(crate::ir::Step::TailCallSelf { argc: 1 });
        let tail_call = make_body(oversized_tail_call, true);
        assert!(!lookup_info(hook, &tail_call, false).is_null());
        assert_eq!(fake_cap_lookup_calls(), 2);
    }

    #[test]
    fn lookup_info_waits_for_the_threshold_after_an_eviction() {
        // A body evicted from the cache does not recompile on its next call
        // however hot it is — it waits for the threshold again, which is what
        // keeps a once-per-frame body from paying a compile per frame. A loop
        // body bypasses the threshold only until it has been evicted once.
        // `fake_evict_lookup` and its own counter: the parallel
        // `..._compiles_loop_bodies_on_the_first_consult` test asserts an
        // absolute count on the shared `FAKE_LOOP_LOOKUP_CALLS`, so the two
        // could not share it.
        FAKE_EVICT_LOOKUP_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
        let hook = JitHook {
            cache: std::ptr::null_mut(),
            lookup: fake_evict_lookup,
            drop_cache: fake_drop_cache,
            helpers: &JIT_SLOW_PATHS as *const JitSlowPaths,
        };
        let body = make_body(vec![crate::ir::Step::Push(Value::Undefined)], true);
        assert!(
            !lookup_info(hook, &body, false).is_null(),
            "a loop body compiles on the first consult"
        );
        // The cache's eviction: fast pointer cleared, eviction counted, and
        // the consult count restarted.
        body.jit_info.set(0);
        body.jit_evictions.set(1);
        body.jit_calls.set(0);
        let consulted = fake_evict_lookup_calls();
        for _ in 0..JIT_COMPILE_THRESHOLD {
            assert!(lookup_info(hook, &body, false).is_null());
        }
        assert_eq!(
            fake_evict_lookup_calls(),
            consulted,
            "an evicted loop body waits for the threshold"
        );
        assert!(
            !lookup_info(hook, &body, false).is_null(),
            "the (K+1)th consult recompiles it"
        );
        assert_eq!(fake_evict_lookup_calls(), consulted + 1);
    }

    #[test]
    fn body_has_loop_detects_back_edges_and_fused_loop_shapes() {
        use crate::ir::Step;
        // A forward jump is not a loop.
        assert!(!crate::ir::body_has_loop(&[
            Step::Push(Value::Undefined),
            Step::Jump(1)
        ]));
        // A jump to an earlier step is a back edge.
        assert!(crate::ir::body_has_loop(&[
            Step::Push(Value::Undefined),
            Step::Jump(0)
        ]));
        // The fused loop head and the register body are implicit loops.
        assert!(crate::ir::body_has_loop(&[Step::FastLoopHead {
            var: crate::ir::FastLoopVar::Counter,
            op: BinaryOp::Add,
            limit: crate::ir::RelLimit::Imm(0.0),
            inc: UpdateOp::Increment,
            body_start: 0,
            after: 0,
        }]));
        assert!(crate::ir::body_has_loop(&[Step::RunRegBody {
            ops: Box::new([])
        }]));
        // A self-tail-call re-enters the body's own entry (the interpreter's
        // TCO loop never re-consults the JIT), so it is a loop too.
        assert!(crate::ir::body_has_loop(&[Step::TailCallSelf { argc: 1 }]));
    }
}
