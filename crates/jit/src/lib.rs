//! JIT backend for the slag bytecode VM.
//!
//! The interpreter already compiles every function/script body to a linear
//! `Vec<runtime::ir::Step>` bytecode (`CompiledBody`). This crate lowers that
//! bytecode to native machine code via Cranelift: `Step` is a stack machine,
//! so each step maps to a small CLIF sequence, and the certified fast loops
//! (`FastLoopHead`/`RunRegBody` on the accumulator counter) lower to real
//! branch instructions with the loop counter in a register.
//!
//! # ABI
//!
//! A compiled body is an `extern "C"` function with this signature:
//!
//! ```ignore
//! fn jit_entry(frame: *mut u64, stack: *mut u64, vm: *mut c_void) -> u64
//! ```
//!
//! - `frame` — the body's frame slots (`frame_size` `Value`s; slot `i` at
//!   `frame[i]`). The caller (the Vm integration) sets it up exactly like
//!   `Vm::setup_frame`: params in `0..arity`, `var` slots `undefined`,
//!   lexical slots the uninitialized marker.
//! - `stack` — the value stack base: the JIT pushes/pops above this pointer,
//!   exactly like the interpreter's `Vec<Value>` with `push`/`pop`. The
//!   caller passes one-past-the-top. A compiled body leaves the stack at its
//!   entry length (balanced pushes/pops; the returned value is popped by the
//!   `Return` step itself).
//! - `vm` — an opaque pointer forwarded to the slow-path helpers.
//!
//! The return value is the body's completion value (`Return` pops it), or
//! `Undefined` when the body falls off the end (matching the interpreter's
//! `Empty` completion for leaf bodies).
//!
//! # Supported subset
//!
//! `JitEngine::compile` returns `None` (fall back to the interpreter) for
//! bodies containing an unsupported step. The scaffold lowers:
//!
//! - Stack ops: `Push` (non-heap constants), `Pop`, `Dup`.
//! - Frame slots: `LoadLocal`, `StoreLocal`/`FusedStoreLocal` (TDZ check when
//!   `ScopeInfo::tdz_store` says the slot is lexical), `InitLocal`,
//!   `Inc`/`Dec`, `UpdateLocal`.
//! - Arithmetic: `Binary`/`BinaryImm` (number fast path inline; everything
//!   else through `JitHelpers::binary_slow`), plus the `LeafOp` register
//!   forms (`BinReg`, `BinImm`, `BinConst`, `BinImmLocal`, `BinAccPop`,
//!   `BinLeftReg`).
//! - Control flow: `Jump`, `JumpIfFalse`/`JumpIfTrue` (and the `Keep`
//!   variants), `JumpIfNullishKeep`/`JumpIfNotNullishKeep`,
//!   `JumpIfLtImm`/`Le`/`Gt`/`GeImm`.
//! - The fused canonical loop: `FastLoopBind`/`FastLoopStore`,
//!   `FastLoopHead` (`FastLoopVar::Slot` and `FastLoopVar::Counter`),
//!   `RunRegBody` (the `LeafOp` register executor), `PushAcc`/`PopAcc`/
//!   `IncAcc`/`DecAcc`.
//! - Member access (through the slow-path helpers): `GetMemberName`/
//!   `GetMemberComputed`, and the member writes `AssignMemberName`/
//!   `AssignMemberComputed` (plain `=` and the compound ops — the
//!   cached-old `Dup`+`Get` sequence the compiler emits for `+=` & co.),
//!   plus the `LeafOp` member forms.
//! - Calls: `CallFast` and the fused `CallFastSlot` (through `call_slow`).
//! - Global/outer bindings: the identifier read/write/update (`LoadIdent`,
//!   `ResolveVarIdent`/`PutVarReference`, `UpdateIdent`) and the
//!   script-level fast-script steps `LoadGlobal`/`StoreGlobal`/
//!   `FusedStoreGlobal` — the global steps inline the direct-mapped
//!   `GlobalValueCell` (validated against the live global's id/generation),
//!   falling back to the helpers on a miss.
//! - Captured bindings (through the env machinery): `LoadContextSlot`/
//!   `StoreContextSlot`/`InitContextSlot`/`UpdateContextSlot` (the
//!   capture-context reads/writes a closure body uses), the per-iteration
//!   forms `LoadPerIteration`/`StorePerIteration`/`UpdatePerIteration`
//!   (captured for-head bindings), and the register forms `LeafOp::LoadContext`/
//!   `BinContext`/`BinCtxReg`/`LoadPerIter`/`BinPerIter`. The JIT leaf
//!   path builds the leaf's own `body_context` from the closure's
//!   environment exactly like the interpreter's `run_leaf_body`.
//! - The reference machinery (an identifier read/write/update through the
//!   env-chain when the binding falls off the fast paths): `GetVarReference`/
//!   `UpdateVarReference`/`PutVarReferenceOp`/`PopVarReference` (beside the
//!   already-lowered `ResolveVarIdent`/`PutVarReference`/`LoadIdent`).
//! - `Return`, and the completion steps: `ResetCompletion`/
//!   `NormalizeCompletion`/`ListBegin`/`ListEnd` are no-ops (the scaffold
//!   assumes function-body semantics, where the completion is discarded
//!   except through `Return`), and `SetCompletion` discards the statement's
//!   value exactly like the interpreter's pop — a no-op there would leave a
//!   slot on the JIT stack and drift it one entry per statement inside a
//!   loop.
//!
//! Everything else (closures, `with`/`try`/`switch`/`using`, generator
//! suspension, iterator machinery, global-reference steps, mapped
//! `arguments`) bails to the interpreter.
//!
//! # Slow paths
//!
//! The JIT inlines the number fast paths (tag checks are 2 instructions on
//! the NaN-boxed `Value`). Anything the inline kernel cannot handle — a
//! non-number binary operand, a relational test on a non-number counter, a
//! member read/write, the TDZ ReferenceError, truthiness of a heap value —
//! calls a [`JitHelpers`] entry point, whose address is baked into the
//! machine code at compile time. The runtime integration fills in real
//! helpers (routing to the interpreter's `apply_binary`/`get_member_name`/
//! etc.); the scaffold's tests provide test doubles. If a body needs a
//! helper that is `None`, compilation bails.
//!
//! # Integration
//!
//! The dependency direction is one-way (`jit` depends on `runtime`), so the
//! Vm never calls into this crate directly: [`install`] populates
//! `Agent::jit_hook` with a [`JitCache`] (a callback registry owned by the
//! runtime), and the runtime's leaf-call path (`Vm::run_jit_leaf`) consults
//! the cache before interpreting a certified body. On a hit it sets up the
//! `frame`/`stack` per the ABI above, calls the entry point, and lands the
//! returned completion value like a leaf call.
//!
//! Known constraints for that integration: helper calls receive the Vm and
//! may reallocate its value stack, so the runtime integration passes the JIT
//! a frame/working area in a private buffer the helpers never see (their
//! own pushes grow the interpreter's stack, never the JIT's raw pointers);
//! the executable pages are W^X (allocated RW, copied, then protected RX);
//! the compiled-body cache is bounded (least-recently-used entries are
//! evicted once it overflows, and eviction is suppressed while a compiled
//! frame is executing, so a running body's entry pointer stays valid); and
//! deep JIT nesting is guarded — beyond `runtime::jit::MAX_JIT_DEPTH` the
//! runtime falls back to the interpreter so the private buffers cannot
//! exhaust the native stack.

pub mod code_buffer;
pub mod compiler;
pub mod helpers;

pub use code_buffer::ExecutableCode;
pub use compiler::JitEngine;
pub use helpers::JitHelpers;

use std::collections::HashMap;
use std::os::raw::c_void;
use std::rc::Rc;

use runtime::ir::CompiledBody;

/// The compiled entry-point ABI: `(frame, stack, vm) -> completion value`.
///
/// `frame`/`stack` point at `Value` (u64) slots — see the crate docs. This
/// is the ABI-invisible mirror of `runtime::jit::JitEntry` (the runtime
/// spells the pointer args `*mut c_void`; the compiled code's signature is
/// generated from the same three pointer-sized params).
pub type JitEntry = unsafe extern "C" fn(frame: *mut u64, stack: *mut u64, vm: *mut c_void) -> u64;

/// The per-body compiled-code metadata the runtime cache returns on a hit
/// (mirrors `runtime::jit::JitCompiledInfo` — `#[repr(C)]`, layout-identical).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JitCompiledInfo {
    /// The entry point (cast to `usize`).
    pub entry: usize,
    /// The body's maximum value-stack depth above the frame, in slots — the
    /// JIT's working area size.
    pub stack_usage: usize,
}

/// A compiled body's executable machine code.
pub struct Compiled {
    /// Held for the allocation's lifetime: the JIT entry pointer points into
    /// this memory, so it must stay alive (and executable) as long as the
    /// `Compiled` is used.
    #[allow(dead_code)]
    pub(crate) code: ExecutableCode,
    pub(crate) info: JitCompiledInfo,
}

impl Compiled {
    /// Run the compiled body against `frame`/`stack`.
    ///
    /// # Safety
    ///
    /// `frame` must point to at least `frame_size` writable `Value` slots set
    /// up per the crate-level ABI docs; `stack` must point to a writable
    /// region the body can push into (one-past-the-top of the caller's value
    /// stack); `vm` must be valid for the compiled body's slow-path helpers.
    pub unsafe fn call(&self, frame: *mut u64, stack: *mut u64, vm: *mut c_void) -> u64 {
        // SAFETY: the caller upholds the ABI contract documented on `call`.
        unsafe { (self.entry())(frame, stack, vm) }
    }

    fn entry(&self) -> JitEntry {
        // SAFETY: `info.entry` was produced from this allocation's own code
        // pointer; a fn pointer is pointer-sized, so the integer round-trip
        // is exact on every supported target (no trampolines).
        unsafe { std::mem::transmute::<usize, JitEntry>(self.info.entry) }
    }
}

/// The compiled-body cache the Vm consults before interpreting a certified
/// leaf: keyed on the `Rc<CompiledBody>` identity, compiled on first use.
/// Each entry holds the body's `Rc` strongly, so the key can never be reused
/// by a different body while its compiled code is cached. The cache is
/// bounded: once it holds `MAX_CACHE_ENTRIES`, inserting a new body evicts
/// the least-recently-used entries (down to `EVICT_TO_ENTRIES`), freeing
/// their executable code and clearing the per-body fast pointer so the next
/// call recompiles. Eviction only runs when no compiled frame is executing
/// (the runtime passes `in_flight` to [`JitCache::lookup`]): a running
/// body's entry pointer stays valid for the call, and the recursion guard
/// (`runtime::jit::MAX_JIT_DEPTH`) bounds how far the cache can overgrow in
/// that window. A body that fails to compile is remembered as a miss so the
/// (expensive) compile attempt happens once, not on every call.
pub struct JitCache {
    engine: JitEngine,
    helpers: JitHelpers,
    entries: HashMap<usize, Entry>,
    /// A monotonic last-use clock (bumped per lookup); the eviction policy
    /// evicts the smallest `last_used` entries.
    clock: u64,
    /// The entry count beyond which an insert (with no frame in flight)
    /// evicts the least-recently-used entries.
    cap: usize,
    /// The entry count an eviction leaves behind (a floor below the cap, so
    /// a burst of new bodies does not thrash the cache one entry at a time).
    evict_to: usize,
}

/// One cached body: the strong `Rc` (pins the body so its identity cannot
/// be reused while cached), the compiled code (or a remembered miss), and
/// the last-use clock for eviction.
struct Entry {
    body: Rc<CompiledBody>,
    compiled: Option<Rc<Compiled>>,
    last_used: u64,
}

/// The cache's capacity: beyond this many entries the least-recently-used
/// bodies are evicted.
pub const MAX_CACHE_ENTRIES: usize = 256;

/// Eviction removes entries down to this floor (half the capacity), so a
/// burst of new bodies does not thrash the cache entry by entry.
pub const EVICT_TO_ENTRIES: usize = 128;

impl JitCache {
    /// A cache whose compile step uses `helpers` as the slow-path table.
    pub fn new(helpers: JitHelpers) -> Result<Self, String> {
        Self::with_capacity(helpers, MAX_CACHE_ENTRIES, EVICT_TO_ENTRIES)
    }

    /// A cache with a custom capacity/eviction floor (test introspection).
    fn with_capacity(helpers: JitHelpers, cap: usize, evict_to: usize) -> Result<Self, String> {
        Ok(Self {
            engine: JitEngine::new()?,
            helpers,
            entries: HashMap::new(),
            clock: 0,
            cap,
            evict_to,
        })
    }

    /// Look up `body`'s compiled code, compiling on first use. Returns a
    /// pointer to the metadata, valid for as long as the entry lives (the
    /// cache evicts only when `in_flight` is false, and clears the body's
    /// fast pointer when it does); null when the body is not
    /// JIT-compilable. `in_flight` is true while another compiled body is
    /// executing: its entry pointer must survive, so no eviction runs.
    pub fn lookup(&mut self, body: &Rc<CompiledBody>, in_flight: bool) -> *const JitCompiledInfo {
        let key = Rc::as_ptr(body) as usize;
        self.clock = self.clock.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.clock;
        } else {
            self.evict_if_needed(in_flight);
            let compiled = self.engine.compile(body, &self.helpers).map(Rc::new);
            self.entries.insert(
                key,
                Entry {
                    body: body.clone(),
                    compiled,
                    last_used: self.clock,
                },
            );
        }
        let entry = &self.entries[&key];
        match &entry.compiled {
            Some(compiled) => &compiled.info,
            None => std::ptr::null(),
        }
    }

    /// Evict the least-recently-used entries down to the floor, unless a
    /// compiled frame is executing (its entry pointer is live on the native
    /// stack) or the cache is under capacity.
    fn evict_if_needed(&mut self, in_flight: bool) {
        if in_flight || self.entries.len() < self.cap {
            return;
        }
        let mut keys: Vec<(usize, u64)> = self
            .entries
            .iter()
            .map(|(key, entry)| (*key, entry.last_used))
            .collect();
        keys.sort_unstable_by_key(|(_, used)| *used);
        let remove = self.entries.len() - self.evict_to;
        for (key, _) in keys.into_iter().take(remove) {
            if let Some(entry) = self.entries.remove(&key) {
                // Clear the per-body fast pointer: the compiled info it
                // points at is freed with the entry, so the next call must
                // reconsult the cache (and recompile).
                entry.body.jit_info.set(0);
            }
        }
    }

    /// The number of distinct bodies compiled so far (test introspection).
    pub fn compiled_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.compiled.is_some())
            .count()
    }
}

/// The runtime's slow-path table as a [`JitHelpers`] table: every field is
/// a real entry point, so no compiled body bails for a missing helper.
fn runtime_helpers() -> JitHelpers {
    let rt = &runtime::jit::JIT_SLOW_PATHS;
    JitHelpers {
        binary_slow: Some(rt.binary_slow),
        concat_strings: Some(rt.concat_strings),
        relational_slow: Some(rt.relational_slow),
        update_value_slow: Some(rt.update_value_slow),
        to_boolean_slow: Some(rt.to_boolean_slow),
        tdz_error: Some(rt.tdz_error),
        get_member_name: Some(rt.get_member_name),
        get_member_computed: Some(rt.get_member_computed),
        set_member_name: Some(rt.set_member_name),
        set_member_computed: Some(rt.set_member_computed),
        call_slow: Some(rt.call_slow),
        leaf_call_probe: Some(rt.leaf_call_probe),
        get_global: Some(rt.get_global),
        set_global: Some(rt.set_global),
        set_global_slot: Some(rt.set_global_slot),
        load_ident: Some(rt.load_ident),
        resolve_var_ident: Some(rt.resolve_var_ident),
        put_var_reference: Some(rt.put_var_reference),
        update_ident: Some(rt.update_ident),
        assign_member_name: Some(rt.assign_member_name),
        assign_member_computed: Some(rt.assign_member_computed),
        set_member_slot: Some(rt.set_member_slot),
        load_context: Some(rt.load_context),
        store_context: Some(rt.store_context),
        init_context: Some(rt.init_context),
        update_context: Some(rt.update_context),
        load_per_iter: Some(rt.load_per_iter),
        store_per_iter: Some(rt.store_per_iter),
        update_per_iter: Some(rt.update_per_iter),
        get_var_reference: Some(rt.get_var_reference),
        update_var_reference: Some(rt.update_var_reference),
        put_var_reference_op: Some(rt.put_var_reference_op),
        pop_var_reference: Some(rt.pop_var_reference),
    }
}

/// Install a JIT cache into `agent`: the runtime's leaf-call path consults
/// it (via `Agent::jit_hook`) before interpreting a certified body. The
/// cache is owned by the hook and freed when the agent drops.
pub fn install(agent: &mut runtime::Agent) -> Result<(), String> {
    let rt = &runtime::jit::JIT_SLOW_PATHS;
    let cache = Box::new(JitCache::new(runtime_helpers())?);
    agent.jit_hook = Some(runtime::jit::JitHook {
        cache: Box::into_raw(cache) as *mut c_void,
        lookup: jit_cache_lookup,
        drop_cache: jit_cache_drop,
        helpers: rt,
    });
    Ok(())
}

unsafe extern "C" fn jit_cache_lookup(
    cache: *mut c_void,
    body: *const c_void,
    in_flight: bool,
) -> *const c_void {
    // SAFETY: the runtime passes the pointer `install` returned and a live
    // `Rc<CompiledBody>` (the caller holds it for the call).
    let cache = unsafe { &mut *(cache as *mut JitCache) };
    let body = unsafe { &*(body as *const Rc<CompiledBody>) };
    cache.lookup(body, in_flight) as *const c_void
}

unsafe extern "C" fn jit_cache_drop(cache: *mut c_void) {
    // SAFETY: the agent calls this once on drop with the pointer `install`
    // returned (the cache's only owner).
    drop(unsafe { Box::from_raw(cache as *mut JitCache) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crux::Value;
    use runtime::ir::{CompiledBody, ScopeInfo, Step};

    /// A certified-style scope for the hand-built test bodies: 2 slots, both
    /// `var`-like (no TDZ), nothing captured.
    fn scope(frame_size: usize) -> ScopeInfo {
        ScopeInfo {
            frame_size,
            arity: 0,
            slots: Default::default(),
            tdz_store: vec![false; frame_size],
            context_names: Vec::new(),
            context_tdz: Vec::new(),
            context_const: Vec::new(),
            context_param: Vec::new(),
            context_slots: Default::default(),
            arguments_slot: None,
            arguments_formals: None,
            this_slot: None,
            args_alias: false,
            annex_b: Vec::new(),
            statement_fns: Vec::new(),
        }
    }

    fn make_body(steps: Vec<Step>, frame_size: usize) -> CompiledBody {
        CompiledBody {
            steps,
            handlers: Vec::new(),
            strict: false,
            scope: Some(scope(frame_size)),
            env_constant: true,
            leaf: false,
            leaf_needs_env: false,
            leaf_uses_env: false,
            leaf_ops: None,
            script_globals: None,
            jit_info: std::cell::Cell::new(0),
        }
    }

    fn helpers_all() -> JitHelpers {
        JitHelpers {
            binary_slow: Some(helpers::test_binary_slow),
            concat_strings: Some(helpers::test_concat_strings),
            relational_slow: Some(helpers::test_relational_slow),
            update_value_slow: Some(helpers::test_update_value_slow),
            to_boolean_slow: Some(helpers::test_to_boolean_slow),
            tdz_error: Some(helpers::test_tdz_error),
            get_member_name: Some(helpers::test_get_member_name),
            get_member_computed: Some(helpers::test_get_member_computed),
            set_member_name: Some(helpers::test_set_member_name),
            set_member_computed: Some(helpers::test_set_member_computed),
            call_slow: Some(helpers::test_call_slow),
            leaf_call_probe: Some(helpers::test_leaf_call_probe),
            get_global: Some(helpers::test_get_global),
            set_global: Some(helpers::test_set_global),
            set_global_slot: Some(helpers::test_set_global_slot),
            load_ident: Some(helpers::test_load_ident),
            resolve_var_ident: Some(helpers::test_resolve_var_ident),
            put_var_reference: Some(helpers::test_put_var_reference),
            update_ident: Some(helpers::test_update_ident),
            assign_member_name: Some(helpers::test_assign_member_name),
            assign_member_computed: Some(helpers::test_assign_member_computed),
            set_member_slot: Some(helpers::test_set_member_slot),
            load_context: Some(helpers::test_load_context),
            store_context: Some(helpers::test_store_context),
            init_context: Some(helpers::test_init_context),
            update_context: Some(helpers::test_update_context),
            load_per_iter: Some(helpers::test_load_per_iter),
            store_per_iter: Some(helpers::test_store_per_iter),
            update_per_iter: Some(helpers::test_update_per_iter),
            get_var_reference: Some(helpers::test_get_var_reference),
            update_var_reference: Some(helpers::test_update_var_reference),
            put_var_reference_op: Some(helpers::test_put_var_reference_op),
            pop_var_reference: Some(helpers::test_pop_var_reference),
        }
    }

    /// A bare (no-helpers) helper table.
    fn helpers_none() -> JitHelpers {
        JitHelpers::none()
    }

    /// Run a compiled body against a fresh frame + stack and return the
    /// completion value. Canary slots around both buffers catch out-of-bounds
    /// writes from the compiled code. A real per-call context is passed (the
    /// compiled code's pending-check reads its `pending` byte at offset 0);
    /// the test doubles never touch the agent/vm pointers.
    fn run(compiled: &Compiled, frame_len: usize) -> u64 {
        const CANARY: u64 = 0xDEAD_BEEF_CAFE_F00D;
        let mut frame = vec![0u64; frame_len + 1];
        frame[frame_len] = CANARY;
        let mut stack = vec![0u64; 65];
        stack[64] = CANARY;
        let mut ctx = runtime::jit::JitCallContext {
            pending: false,
            error: None,
            agent: std::ptr::null_mut(),
            vm: std::ptr::null_mut(),
            // A null global routes the inline `LoadGlobal` fast path to the
            // helper (the test doubles), so the bare ctx stays safe.
            global_object: std::ptr::null_mut(),
            global_value_cells: std::ptr::null_mut(),
            member_value_cells: std::ptr::null_mut(),
            clean_chain: false,
            buf_end: std::ptr::null_mut(),
            leaf_epoch: 0,
            leaf_call_cache: runtime::jit::LeafCallSiteCache::empty(),
        };
        // Safety: the buffers outlive the call; `vm` is never dereferenced by
        // the scaffold's test helpers.
        let result = unsafe {
            compiled.call(
                frame.as_mut_ptr(),
                stack.as_mut_ptr(),
                (&mut ctx as *mut runtime::jit::JitCallContext) as *mut std::os::raw::c_void,
            )
        };
        assert!(!ctx.pending, "a test-double helper set the error flag");
        assert_eq!(frame[frame_len], CANARY, "frame overrun");
        assert_eq!(stack[64], CANARY, "stack overrun");
        result
    }

    #[test]
    fn compile_and_run_binary_add() {
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Push(Value::Number(2.0)),
                Step::Binary(syntax::ast::BinaryOp::Add),
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        let bits = run(&compiled, 0);
        assert_eq!(bits, Value::Number(3.0).bits());
    }

    #[test]
    fn compile_and_run_fast_counter_loop() {
        // `var i = 0; var n = 0; for (; i < 1000; i++) { n += i }` on the
        // accumulator path — the exact certified shape the compiler emits:
        // FastLoopBind, the fused initial test, the step-path body (PushAcc),
        // the FastLoopHead back edge, and the counter store.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::ResetCompletion,
                Step::Push(Value::Number(0.0)),
                Step::InitLocal { slot: 0 },
                Step::Push(Value::Number(0.0)),
                Step::InitLocal { slot: 1 },
                Step::FastLoopBind {
                    var: runtime::ir::FastLoopVar::Slot(0),
                },
                Step::JumpIfLtImm {
                    slot: 0,
                    imm: 1000.0,
                    target: 12,
                },
                Step::LoadLocal { slot: 1 },
                Step::PushAcc,
                Step::Binary(syntax::ast::BinaryOp::Add),
                Step::StoreLocal { slot: 1 },
                Step::FastLoopHead {
                    var: runtime::ir::FastLoopVar::Counter,
                    op: syntax::ast::BinaryOp::LessThan,
                    imm: 1000.0,
                    inc: syntax::ast::UpdateOp::Increment,
                    body_start: 7,
                    after: 12,
                },
                Step::FastLoopStore {
                    var: runtime::ir::FastLoopVar::Slot(0),
                },
                Step::NormalizeCompletion,
                Step::LoadLocal { slot: 1 },
                Step::Return,
            ],
            2,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        let bits = run(&compiled, 2);
        // sum(0..1000) — the counter runs 0..999, then the head's test fails
        // at 1000 and the counter is stored back.
        assert_eq!(bits, Value::Number(499_500.0).bits());
    }

    #[test]
    fn register_body_push_acc_spills_the_accumulator() {
        // `acc = 1; push acc; acc = 2; acc = pop + acc; return acc` — the
        // Cut 35 slice 10 spill shape (`LeafOp::PushAcc` pushes the
        // ACCUMULATOR, not the loop counter; the counter push is
        // `Step::PushAcc`, which a register body reads via `LoadCounter`).
        // Expected 3; pushing the counter (seeded 0) would return 2.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![Step::RunRegBody {
                ops: vec![
                    runtime::ir::LeafOp::LoadConst(Value::Number(1.0)),
                    runtime::ir::LeafOp::PushAcc,
                    runtime::ir::LeafOp::LoadConst(Value::Number(2.0)),
                    runtime::ir::LeafOp::BinAccPop {
                        op: syntax::ast::BinaryOp::Add,
                    },
                    runtime::ir::LeafOp::ReturnAcc,
                ]
                .into(),
            }],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(3.0).bits());
    }

    #[test]
    fn compile_and_run_register_loop_body() {
        // The register-lowered body: `n = n + 1` (LoadReg + BinImmLocal +
        // StoreReg) inside the counter loop, i.e. a `RunRegBody` body.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::ResetCompletion,
                Step::Push(Value::Number(0.0)),
                Step::InitLocal { slot: 0 },
                Step::FastLoopBind {
                    var: runtime::ir::FastLoopVar::Slot(0),
                },
                Step::JumpIfLtImm {
                    slot: 0,
                    imm: 10.0,
                    target: 7,
                },
                Step::RunRegBody {
                    ops: vec![
                        runtime::ir::LeafOp::LoadReg {
                            slot: 1,
                            tdz: false,
                        },
                        runtime::ir::LeafOp::BinImmLocal {
                            op: syntax::ast::BinaryOp::Add,
                            slot: 1,
                            tdz: false,
                            imm: 1.0,
                        },
                        runtime::ir::LeafOp::StoreReg {
                            slot: 1,
                            tdz: false,
                        },
                    ]
                    .into_boxed_slice(),
                },
                Step::FastLoopHead {
                    var: runtime::ir::FastLoopVar::Counter,
                    op: syntax::ast::BinaryOp::LessThan,
                    imm: 10.0,
                    inc: syntax::ast::UpdateOp::Increment,
                    body_start: 5,
                    after: 7,
                },
                Step::FastLoopStore {
                    var: runtime::ir::FastLoopVar::Slot(0),
                },
                Step::NormalizeCompletion,
                Step::LoadLocal { slot: 1 },
                Step::Return,
            ],
            2,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        let bits = run(&compiled, 2);
        assert_eq!(bits, Value::Number(10.0).bits());
    }

    #[test]
    fn compile_and_run_control_flow() {
        // `if (true) { 42 } else { 0 }` — the truthiness inline path (a
        // Boolean tag) plus the forward branch.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Boolean(true)),
                Step::JumpIfFalse(4),
                Step::Push(Value::Number(42.0)),
                Step::Jump(5),
                Step::Push(Value::Number(0.0)),
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(42.0).bits());

        let body = make_body(
            vec![
                Step::Push(Value::Boolean(false)),
                Step::JumpIfFalse(4),
                Step::Push(Value::Number(42.0)),
                Step::Jump(5),
                Step::Push(Value::Number(0.0)),
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(0.0).bits());
    }

    #[test]
    fn slow_binary_uses_the_helper() {
        // `BinaryOp::In` is not in the inline set — the whole op routes
        // through `binary_slow`, whose test double returns 42.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Push(Value::Number(2.0)),
                Step::Binary(syntax::ast::BinaryOp::In),
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(42.0).bits());
    }

    #[test]
    fn missing_helper_bails() {
        let engine = JitEngine::new().expect("native isa");
        // `Binary` needs `binary_slow` (a string operand is possible); with
        // no helpers the compile must bail to the interpreter.
        let body = make_body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Push(Value::Number(2.0)),
                Step::Binary(syntax::ast::BinaryOp::Add),
                Step::Return,
            ],
            0,
        );
        assert!(engine.compile(&body, &helpers_none()).is_none());
    }

    #[test]
    fn unsupported_step_bails() {
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![Step::Push(Value::Number(1.0)), Step::Throw, Step::Return],
            0,
        );
        assert!(engine.compile(&body, &helpers_all()).is_none());
    }

    #[test]
    fn compile_reports_the_stack_usage() {
        // `Push, Push, Binary, Return`: the depth peaks at 2 (two operands
        // live before the `Binary` consumes one).
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Push(Value::Number(2.0)),
                Step::Binary(syntax::ast::BinaryOp::Add),
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(compiled.info.stack_usage, 2);
        assert_ne!(compiled.info.entry, 0);
    }

    #[test]
    fn cache_lookup_is_stable_and_keyed_by_identity() {
        let mut cache = JitCache::new(helpers_all()).expect("isa");
        let body = std::rc::Rc::new(make_body(
            vec![Step::Push(Value::Number(1.0)), Step::Return],
            0,
        ));
        let p1 = cache.lookup(&body, false);
        assert!(!p1.is_null(), "a supported body compiles");
        let p2 = cache.lookup(&body, false);
        assert_eq!(p1, p2, "a cached body returns the same pointer");
        // SAFETY: the single entry is under the capacity, so nothing evicts
        // it and the pointer stays valid.
        let info = unsafe { &*p1 };
        assert_eq!(info.stack_usage, 1, "one push above the entry stack");
        assert_ne!(info.entry, 0);
    }

    #[test]
    fn cache_returns_null_for_an_unsupported_body() {
        let mut cache = JitCache::new(helpers_all()).expect("isa");
        let body = std::rc::Rc::new(make_body(
            vec![Step::Push(Value::Undefined), Step::Throw],
            0,
        ));
        assert!(cache.lookup(&body, false).is_null());
        // A failed compile is remembered, so the (expensive) attempt does
        // not repeat on every call.
        assert!(cache.lookup(&body, false).is_null());
        assert_eq!(cache.compiled_count(), 0);
    }

    #[test]
    fn cache_evicts_least_recently_used_bodies() {
        // cap 2, floor 1: A, B, then A again (hot), then C — B is the LRU
        // and must go; A stays; C lands. The evicted body's per-body fast
        // pointer is cleared, so a later call recompiles it.
        let mut cache = JitCache::with_capacity(helpers_all(), 2, 1).expect("isa");
        let body = |n: f64| {
            std::rc::Rc::new(make_body(
                vec![Step::Push(Value::Number(n)), Step::Return],
                0,
            ))
        };
        let a = body(1.0);
        let b = body(2.0);
        let c = body(3.0);
        assert!(!cache.lookup(&a, false).is_null());
        assert!(!cache.lookup(&b, false).is_null());
        assert!(!cache.lookup(&a, false).is_null()); // A is now most-recent
        assert!(!cache.lookup(&c, false).is_null());
        assert_eq!(cache.compiled_count(), 2, "B was evicted");
        // B's fast pointer was cleared by the eviction...
        assert_eq!(b.jit_info.get(), 0);
        // ...and a later call recompiles it fresh (evicting the LRU, A).
        assert!(!cache.lookup(&b, false).is_null());
        assert_eq!(cache.compiled_count(), 2, "recompiled B evicted the LRU");
        assert_eq!(a.jit_info.get(), 0, "A's fast pointer was cleared");
    }

    #[test]
    fn cache_skips_eviction_while_a_frame_is_in_flight() {
        // While a compiled body is executing, `lookup` must not evict: the
        // running body's entry pointer stays live on the native stack.
        // cap 2, floor 1; the in-flight insert overflows, and the eviction
        // happens only once the frame leaves.
        let mut cache = JitCache::with_capacity(helpers_all(), 2, 1).expect("isa");
        let body = |n: f64| {
            std::rc::Rc::new(make_body(
                vec![Step::Push(Value::Number(n)), Step::Return],
                0,
            ))
        };
        let a = body(1.0);
        let b = body(2.0);
        let c = body(3.0);
        let d = body(4.0);
        assert!(!cache.lookup(&a, false).is_null());
        assert!(!cache.lookup(&b, false).is_null());
        // A frame is in flight: inserting C must not evict A or B.
        assert!(!cache.lookup(&c, true).is_null());
        assert_eq!(cache.compiled_count(), 3, "no eviction while in flight");
        // Once the frame leaves, the next insert evicts down to the floor.
        assert!(!cache.lookup(&d, false).is_null());
        assert_eq!(
            cache.compiled_count(),
            2,
            "eviction resumes after the frame"
        );
        assert_eq!(b.jit_info.get(), 0, "B was the LRU and got evicted");
    }

    #[test]
    fn installed_jit_recursion_guard_falls_back_to_interpreter() {
        // At the depth cap the JIT path bails to the interpreter: the body
        // still runs (correct result) but nothing new compiles.
        let (value, compiled) = with_jit_agent(|agent| {
            agent.jit_depth = runtime::jit::MAX_JIT_DEPTH;
            agent
                .run_script("function f(x) { return x + 1; } f(41);")
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(42.0));
        assert_eq!(compiled, 0, "the depth cap skips the JIT entirely");
    }

    #[test]
    fn call_fast_lowers_and_passes_args_by_pointer() {
        // `[this, callee, 1, 2, 3] -> CallFast(argc=3)`: the test double
        // sums the numeric arguments, proving the `args` pointer/`argc` ABI.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Undefined),
                Step::Push(Value::Undefined),
                Step::Push(Value::Number(1.0)),
                Step::Push(Value::Number(2.0)),
                Step::Push(Value::Number(3.0)),
                Step::CallFast {
                    argc: 3,
                    direct_eval: false,
                },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(6.0).bits());
    }

    #[test]
    fn call_fast_direct_eval_bails() {
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![Step::CallFast {
                argc: 0,
                direct_eval: true,
            }],
            0,
        );
        assert!(engine.compile(&body, &helpers_all()).is_none());
    }

    /// Run `f` with a fresh agent that has the JIT hook installed; returns
    /// the completion value and the number of distinct bodies the cache
    /// compiled. The cache lives on the test's stack, so `drop_cache` is a
    /// no-op and the hook is cleared before the agent drops (the agent's
    /// Drop would otherwise free a non-heap pointer).
    fn with_jit_agent(f: impl FnOnce(&mut runtime::Agent) -> Value) -> (Value, usize) {
        extern "C" fn noop_drop(_cache: *mut c_void) {}
        let mut agent = runtime::Agent::new();
        agent.initialize_host_defined_realm().expect("realm");
        let mut cache = JitCache::new(runtime_helpers()).expect("isa");
        agent.jit_hook = Some(runtime::jit::JitHook {
            cache: (&mut cache as *mut JitCache) as *mut c_void,
            lookup: jit_cache_lookup,
            drop_cache: noop_drop,
            helpers: &runtime::jit::JIT_SLOW_PATHS,
        });
        let value = f(&mut agent);
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        (value, compiled)
    }

    #[test]
    fn installed_jit_runs_a_member_callee() {
        // `return o.f(1) + 1` — a member callee (plain `CallFast`), no loop.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(o) { return o.f(1) + 1; } f({ f: function (x) { return x + 1; } });",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(3.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_slot_callee() {
        // `return g(x) + 1` — a param callee (fused `CallFastSlot`), no loop.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(g, x) { return g(x) + 1; } f(function (x) { return x + 1; }, 41);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(43.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_loop_with_slot_calls() {
        // `f(g, n) { var s = 0; for (...) { s += g(i); } }` — a slot call
        // inside a general loop (a call disqualifies the fast-loop shape).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(g, n) { var s = 0; for (var i = 0; i < n; i++) { s += g(i); } return s; }\n\
                     f(function (x) { return x + 1; }, 100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(5050.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_loop_with_member_calls() {
        // The general path: `f`'s body contains a plain `CallFast` (a loop
        // calling `o.f(i)`), so it is certified but not a leaf — it runs
        // through `ordinary_call` → `run_compiled_body`, whose JIT hook runs
        // the compiled body. The callee is a certified leaf, so the nested
        // call takes the leaf-path JIT. Result: sum 1..100 = 5050.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(o, n) { var s = 0; for (var i = 0; i < n; i++) { s += o.f(i); } return s; }\n\
                     f({ f: function (x) { return x + 1; } }, 100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(5050.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_member_probe_misses_on_mid_run_mutation() {
        // The compiled `GetMemberName` probe validates the value cell
        // against the receiver's LIVE generation: the `o.f = 2` store at
        // `i == 50` bumps it, so the remaining reads must miss to the
        // helper (a stale cell would keep serving 1). Expected:
        // 50 * 1 + 50 * 2 = 150.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(o, n) { var s = 0; for (var i = 0; i < n; i++) { if (i === 50) { o.f = 2; } s += o.f; } return s; }\n\
                     f({ f: 1 }, 100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(150.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_with_gc_stress_keeps_the_buffer_rooted() {
        // The private frame/working buffer must be traced for the JIT run's
        // duration: `s` (a string only the buffer references) would be swept
        // by a helper-triggered per-allocation stress collection. The loop
        // concats 1000 times, so collections run mid-body.
        let (value, _) = with_jit_agent(|agent| {
            agent.set_gc_stress(true);
            agent
                .run_script(
                    "function f(x) { var s = x; for (var i = 0; i < 1000; i++) { s += x; } return s.length; } f('x');",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1001.0));
    }

    #[test]
    fn installed_jit_with_gc_stress_roots_the_general_frame() {
        // The general-path frame lives in `vm.frame`, not the private
        // working area — a computed string stored to a frame slot must be
        // traced across the loop's per-allocation stress collections, with
        // calls (the general path) interleaved.
        let (value, _) = with_jit_agent(|agent| {
            agent.set_gc_stress(true);
            agent
                .run_script(
                    "function f(o, n) { var s = o.name; for (var i = 0; i < n; i++) { s += o.f(i); } return s.length; }\n\
                     f({ name: '', f: function (x) { return x + 1; } }, 1000);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(2893.0));
    }

    #[test]
    fn global_load_store_lower() {
        // The test doubles: `get_global` returns 42; `set_global` returns
        // the stored value (discarded by `StoreGlobal`).
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(vec![Step::LoadGlobal { name: 1 }, Step::Return], 0);
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(42.0).bits());

        let body = make_body(
            vec![
                Step::Push(Value::Number(3.0)),
                Step::StoreGlobal { name: 2 },
                Step::Push(Value::Number(5.0)),
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(5.0).bits());
    }

    #[test]
    fn ident_load_store_update_lower() {
        // The test doubles: `load_ident` returns 42, `put_var_reference`
        // returns the value, `update_ident` returns old + 1.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(vec![Step::LoadIdent { name: 1 }, Step::Return], 0);
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(42.0).bits());

        // `ResolveVarIdent` (no stack effect) + value + `PutVarReference`
        // (pops the value, re-pushes it as the assignment's result).
        let body = make_body(
            vec![
                Step::ResolveVarIdent { name: 1 },
                Step::Push(Value::Number(7.0)),
                Step::PutVarReference,
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(7.0).bits());

        // `UpdateIdent`: pop the old value, push the result.
        let body = make_body(
            vec![
                Step::Push(Value::Number(5.0)),
                Step::UpdateIdent {
                    name: 1,
                    op: syntax::ast::UpdateOp::Increment,
                    prefix: true,
                },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(6.0).bits());
    }

    #[test]
    fn installed_jit_runs_a_body_with_globals() {
        // `f` reads and writes a declared top-level `var` through the
        // direct-mapped global cells (`LoadGlobal`/`StoreGlobal` route to
        // the interpreter's cell fast path). Result: 100 * 10 = 1000.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var g = 10; function f() { var s = 0; for (var i = 0; i < 100; i++) { s += g; } g = s; return s; } f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1000.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_global_read_inline_misses_on_mid_run_mutation() {
        // The inline `LoadGlobal` fast path validates the value cell against
        // the global object's LIVE generation: the store at `i == 50` bumps
        // it, so the remaining reads must miss to `get_global` (a stale ctx
        // snapshot would keep serving the pre-store value). Expected:
        // 50 * 1 + 50 * 2 = 150.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var g = 1; function f(n) { var s = 0; for (var i = 0; i < n; i++) { if (i === 50) { g = 2; } s += g; } return s; } f(100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(150.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_global_store_warm_reads_follow_the_store() {
        // A global written and read in the SAME compiled body: the first
        // iteration's `g = i` misses (the cell is empty) and falls to the
        // cached `store_global_value`, which mirrors the JIT cell; the rest
        // take the compiled `StoreGlobal` fast path (cell write + validated
        // slot write). A cached store that did NOT mirror the cell would
        // leave the cell at `0` and every `s += g` fast read would add 0.
        // Expected: 0 + 1 + ... + 99 = 4950.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var g = 0; function f(n) { var s = 0; for (var i = 0; i < n; i++) { g = i; s += g; } return s; } f(100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(4950.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_global_store_then_read_same_body() {
        // The store-then-read shape in one compiled body: every iteration's
        // `g = i` store (fast path after the first) must be visible to the
        // next iteration's `s += g` fast read AND to the final `return g` —
        // a store that did not update the cell would leave them reading the
        // pre-store value. Expected: g = 9 after the loop.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var g = 0; function f(n) { var s = 0; for (var i = 0; i < n; i++) { g = i; s += g; } return g; } f(10);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(9.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_ident_read_with_scope_shadow_is_respected() {
        // A certified closure created inside a `with` reads a name the with
        // object shadows: its env chain contains a `with` scope, so the
        // compiled `LoadIdent` probe must be gated off (the cell for `x`
        // holds the global property's 1, not the with object's 2). The
        // per-call `clean_chain` flag makes the probe miss to `load_ident`,
        // which resolves through the with env.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var x = 1; with ({ x: 2 }) { var f = function () { return x; }; } f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(2.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_ident_read_let_shadow_invalidates_the_cell() {
        // A global LEXICAL binding shadows an existing CONFIGURABLE data
        // property (a plain `var` is non-configurable, so this needs
        // `defineProperty`): the first script's calls warm the `x` cell (the
        // first `f()` misses and `load_ident` records the property; the
        // second reads it). The second script's `let x` then shadows the
        // property in the global env's DECLARATIVE record — which does not
        // touch the global object, so instantiating it must bump the
        // generation to invalidate the cell. Without the bump, the compiled
        // probe would keep serving the property's 1.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "Object.defineProperty(globalThis, 'x', { value: 1, writable: true, configurable: true }); \
                     function f() { return x; } f(); f();",
                )
                .expect("first script");
            agent.run_script("let x = 2; f();").expect("second script")
        });
        assert_eq!(value.as_number(), Some(2.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_inline_leaf_call_this_using_callee_falls_back() {
        // A this-using leaf (a `this` slot) cannot run in-frame — the probe
        // rejects it and the call falls back to `call_slow`, whose
        // interpreter leaf-inline binds `this`. Result:
        // sum(41 + i, i in 0..100) = 4100 + 4950 = 9050.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(o, n) { var s = 0; for (var i = 0; i < n; i++) { s += o.f(i); } return s; }\n\
                     f({ f: function (x) { return this.v + x; }, v: 41 }, 100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(9050.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_inline_leaf_call_captured_callee_falls_back() {
        // An env-using leaf (captures `y`) cannot run in-frame — the probe
        // rejects it and the interpreter's leaf-inline resolves the capture
        // through the closure's environment. Result: sum(i + 10) = 5950.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var y = 10; function f(g, n) { var s = 0; for (var i = 0; i < n; i++) { s += g(i); } return s; }\n\
                     f(function (x) { return x + y; }, 100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(5950.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_inline_leaf_call_with_var_slot_builds_the_frame() {
        // A leaf with a `var` slot (frame_size > arity, so the arguments
        // cannot alias the frame): the probe builds the frame above the
        // arguments, the var initializes to undefined, and the body's
        // arithmetic uses it. Result: 2 * sum(i + 1, i in 0..100) = 10100.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(g, n) { var s = 0; for (var i = 0; i < n; i++) { s += g(i); } return s; }\n\
                     f(function (x) { var t = x + 1; return t * 2; }, 100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(10100.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn compound_member_assign_lowers() {
        // The test doubles: `old + value` for a compound op, else `value`.
        let engine = JitEngine::new().expect("native isa");
        // Named compound: [object, old=5, value=3] -> 8.
        let body = make_body(
            vec![
                Step::Push(Value::Undefined),
                Step::Push(Value::Number(5.0)),
                Step::Push(Value::Number(3.0)),
                Step::AssignMemberName {
                    name: 1,
                    op: syntax::ast::AssignOp::AddAssign,
                },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(8.0).bits());

        // Named plain: [object, value=7] -> 7 (no old popped).
        let body = make_body(
            vec![
                Step::Push(Value::Undefined),
                Step::Push(Value::Number(7.0)),
                Step::AssignMemberName {
                    name: 1,
                    op: syntax::ast::AssignOp::Assign,
                },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(7.0).bits());

        // Computed compound: [object, key, old=5, value=2] -> 7.
        let body = make_body(
            vec![
                Step::Push(Value::Undefined),
                Step::Push(Value::Undefined),
                Step::Push(Value::Number(5.0)),
                Step::Push(Value::Number(2.0)),
                Step::AssignMemberComputed {
                    op: syntax::ast::AssignOp::AddAssign,
                },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(7.0).bits());
    }

    #[test]
    fn context_steps_lower() {
        // The test doubles: `load_context` returns 42, `update_context` 43;
        // the stores echo the stored value.
        let engine = JitEngine::new().expect("native isa");
        // LoadContextSlot pushes the read value: [LoadContextSlot] -> 42.
        let body = make_body(
            vec![Step::LoadContextSlot { depth: 0, index: 0 }, Step::Return],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(42.0).bits());

        // StoreContextSlot pops the value and discards it.
        let body = make_body(
            vec![
                Step::Push(Value::Number(7.0)),
                Step::StoreContextSlot { depth: 0, index: 1 },
                Step::Push(Value::Number(3.0)),
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(3.0).bits());

        // InitContextSlot pops the value and discards it.
        let body = make_body(
            vec![
                Step::Push(Value::Number(7.0)),
                Step::InitContextSlot { index: 2 },
                Step::Push(Value::Number(4.0)),
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(4.0).bits());

        // UpdateContextSlot pushes the updated value (the double's 43).
        let body = make_body(
            vec![
                Step::UpdateContextSlot {
                    depth: 0,
                    index: 0,
                    op: syntax::ast::UpdateOp::Increment,
                    prefix: false,
                },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(43.0).bits());
    }

    #[test]
    fn per_iteration_steps_lower() {
        // The test doubles: `load_per_iter` returns 44, `update_per_iter` 45;
        // the store echoes the stored value.
        let engine = JitEngine::new().expect("native isa");
        // LoadPerIteration pushes the read value: -> 44.
        let body = make_body(
            vec![Step::LoadPerIteration { depth: 0, index: 0 }, Step::Return],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(44.0).bits());

        // StorePerIteration pops the value and discards it.
        let body = make_body(
            vec![
                Step::Push(Value::Number(7.0)),
                Step::StorePerIteration { depth: 0, index: 1 },
                Step::Push(Value::Number(3.0)),
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(3.0).bits());

        // UpdatePerIteration pushes the updated value (the double's 45).
        let body = make_body(
            vec![
                Step::UpdatePerIteration {
                    depth: 0,
                    index: 0,
                    op: syntax::ast::UpdateOp::Increment,
                    prefix: false,
                },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(45.0).bits());
    }

    #[test]
    fn reference_machinery_lowers() {
        // The test doubles: `get_var_reference` returns 46, the update
        // returns `old + 1`, the compound `old + value`.
        let engine = JitEngine::new().expect("native isa");
        // GetVarReference pushes the read value: -> 46.
        let body = make_body(
            vec![
                Step::ResolveVarIdent { name: 1 },
                Step::GetVarReference,
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(46.0).bits());

        // UpdateVarReference pops the old value and pushes the updated one.
        let body = make_body(
            vec![
                Step::Push(Value::Number(5.0)),
                Step::UpdateVarReference {
                    op: syntax::ast::UpdateOp::Increment,
                    prefix: false,
                },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(6.0).bits());

        // PutVarReferenceOp pops value + old, pushes `old op value`.
        let body = make_body(
            vec![
                Step::Push(Value::Number(5.0)),
                Step::Push(Value::Number(3.0)),
                Step::PutVarReferenceOp {
                    op: syntax::ast::AssignOp::AddAssign,
                },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(8.0).bits());

        // PopVarReference has no value-stack effect.
        let body = make_body(
            vec![
                Step::ResolveVarIdent { name: 1 },
                Step::PopVarReference,
                Step::Push(Value::Number(7.0)),
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(7.0).bits());
    }

    #[test]
    fn installed_jit_runs_a_compound_member_assign() {
        // `o.x += 1` through the real runtime machinery.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script("function f(o) { o.x += 1; return o.x; } f({ x: 41 });")
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(42.0));
        assert!(compiled >= 1, "{compiled} bodies");

        // The loop form: the interpreter's `SetCompletion` pops the
        // statement's value, and the JIT must discard it too — a leftover
        // slot per iteration drifts the working area past the buffer (the
        // bench crash, reproduced here at the failing iteration count).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(o, n) { var s = 0; for (var i = 0; i < n; i++) { o.x += 1; s += o.x; } return s; }\n\
                     f({ x: 0 }, 100000);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(5000050000.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_captured_var_body() {
        // A closure reads/writes a captured binding through the capture
        // context (`LoadContextSlot`/`StoreContextSlot`/`UpdateContextSlot`):
        // the JIT leaf path must build the leaf's own `body_context` from
        // the closure's environment, exactly like `run_leaf_body`, or the
        // helpers would resolve the caller's env.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function make() { var x = 41; return function f() { return x + 1; }; }\n\
                     var f = make(); f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(42.0));
        assert!(compiled >= 1, "{compiled} bodies");

        // A captured write (`x = x + 41` reads then stores) and the fused
        // update (`++x` reads, updates, stores, returns the new value).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function make() { var x = 1; return function f() { x = x + 41; return ++x; }; }\n\
                     var f = make(); f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(43.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_per_iteration_body() {
        // A closure capturing a certified `for (let i...)` head reads the
        // fresh per-iteration binding (`LoadPerIteration`/`LeafOp::LoadPerIter`
        // through the per-iteration env machinery).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function make() { var fns = []; for (let i = 0; i < 3; i++) { fns.push(function () { return i * 10; }); } return fns[2]; }\n\
                     make()();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(20.0));
        assert!(compiled >= 1, "{compiled} bodies");

        // The fused update (`++i` → `UpdatePerIteration`).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function make() { var fns = []; for (let i = 0; i < 3; i++) { fns.push(function () { return ++i; }); } return fns[2]; }\n\
                     make()();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(3.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_certified_leaf_body() {
        // The leaf path: the script body bails (its `CallFastGlobal` step is
        // unsupported), so the interpreter runs it and the leaf-inline path
        // hands the certified callee's run to the JIT. The counter loop and
        // the member access both route through the real runtime slow-path
        // table; a miscompile would surface as a wrong result.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(n) { var s = 0; for (var i = 0; i < n; i++) { s += i; } return s; }\n\
                     f(100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(4950.0));
        assert!(compiled >= 1, "{compiled} bodies");

        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script("function g(o) { var v = o.x; o.x = 42; return v; } g({ x: 41 });")
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(41.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_fused_global_call_store() {
        // `s = g(i)` inside a loop: the compiler fuses the arg loads, the
        // global-callee call, and the store into one `CallFastGlobalStore`
        // step, whose handler passes the caller-frame argument base. The
        // enclosing body bails (the fused step is unsupported), so the
        // interpreter runs it and the fused call site hands the certified
        // callee's run to the JIT with the args materialized from the
        // frame.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function g(x) { return x + 1; }\n\
                     function f(n) { var s = 0; for (var i = 0; i < 100; i++) { s = g(i); } return s; }\n\
                     f(100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(100.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_fused_slot_call_store() {
        // The param-callee twin: `s = g(i)` fuses into `CallFastSlotStore`
        // (the callee comes from the frame slot); the anonymous callee's
        // body is a certified leaf, JIT-run from the fused call site's
        // caller-frame argument base.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(g, n) { var s = 0; for (var i = 0; i < n; i++) { s = g(i); } return s; }\n\
                     f(function (x) { return x + 1; }, 100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(100.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_tail_call_to_a_leaf() {
        // `return g(x)` in tail position routes through `tail_call_shared`,
        // whose leaf path hands the certified callee's run to the JIT. The
        // driver is called 1000 times, so the TCO fires a thousand JIT
        // runs; the driver body bails (its tail-call step is unsupported)
        // and runs on the interpreter.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function g(x) { return x + 1; }\n\
                     function f(x) { return g(x); }\n\
                     var s = 0; for (var i = 0; i < 1000; i++) { s = f(i); } s",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1000.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_construct_leaf() {
        // `new C(5)`: C's body is a construct-inline certified leaf, so the
        // certified construct path (`run_leaf_construct`) materializes the
        // construct args and hands the run to the JIT; the base-constructor
        // result rule (an object/function return wins, else `this`) lands
        // the constructed object.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script("function C(x) { this.v = x; } new C(5).v")
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(5.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_leaf_cache_reprobes_on_callee_change() {
        // Cut 39: the per-call-site leaf cache re-probes when the callee at
        // a site changes — the cached record's identity check is the
        // callee's full NaN-box bits, so a cached `g` verdict must not serve
        // `h`'s calls. `f` swaps its local `c` between the two leaf params
        // at the loop midpoint (a slot store, no helper in between); without
        // the identity gate `g`'s body would run for every `h` call and the
        // loop would land 100 instead of 101.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function g(x) { return x + 1; }\n\
                     function h(x) { return x + 2; }\n\
                     function f(a, b, n) { var c = a; var s = 0; for (var i = 0; i < n; i++) { if (i === 50) { c = b; } s = c(i); } return s; }\n\
                     f(g, h, 100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(101.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_leaf_cache_revalidates_after_a_disturbing_helper() {
        // Cut 39: a slow-path helper that re-enters the interpreter (here
        // the accessor setter behind `o.x = s`) bumps the leaf-eligibility
        // epoch, so a cached leaf verdict is re-probed — never blindly
        // reused — after the disturbance. The loop alternates the leaf call
        // and the member store, so every iteration exercises the bump +
        // re-probe cycle; the results stay exact.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var count = 0;\n\
                     var o = {};\n\
                     Object.defineProperty(o, 'x', { set: function (v) { count += 1; } });\n\
                     function g(x) { return x + 1; }\n\
                     function f(n) { var s = 0; for (var i = 0; i < n; i++) { s = g(i); o.x = s; } return s; }\n\
                     var r = f(100); r === 100 && count === 100;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_boolean(), Some(true));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_compound_assign_fast_path_stays_warm() {
        // Cut 40: `o.x += 1` on a plain object with a warm value cell
        // computes the new value inline and writes the property vector in
        // place (no generation bump), and the cell refresh keeps the
        // following `s += o.x` read on the native probe — a stale cell
        // would land 5050 - 100 = 4950 instead of 5050.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(o, n) { var s = 0; for (var i = 0; i < n; i++) { o.x += 1; s += o.x; } return s; }\n\
                     f({ x: 0 }, 100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(5050.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_member_store_respects_non_writable() {
        // Cut 40: the fast path's writable gate — `o.x = 5` on a
        // non-writable data property must silently fail (sloppy), so the
        // read keeps seeing 1. The write helper's authoritative check is
        // what blocks it (the value cell never checks writability); a
        // direct vector write would land 50 instead of 10.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var o = {};\n\
                     Object.defineProperty(o, 'x', { value: 1, writable: false });\n\
                     function f(o, n) { var s = 0; for (var i = 0; i < n; i++) { o.x = 5; s += o.x; } return s; }\n\
                     f(o, 10);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(10.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_member_compound_runs_the_setter() {
        // Cut 40: an accessor property never warms the value cell, so
        // `o.x += 1` stays on the full helper — the setter must run every
        // iteration (the fast path must not bypass it).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var count = 0;\n\
                     var o = {};\n\
                     Object.defineProperty(o, 'x', { get: function () { return 1; }, set: function (v) { count += 1; } });\n\
                     function f(o, n) { var s = 0; for (var i = 0; i < n; i++) { o.x += 1; s += o.x; } return s; }\n\
                     var r = f(o, 10); r === 10 && count === 10;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_boolean(), Some(true));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_fast_loop_member_read_runs_the_getter() {
        // Cut 40: the register body's fused member read (`s += o.x` in a
        // certified loop lowers to `GetMemberNameLocal`) shares the
        // member-cell probe — an accessor never warms the cell, so the
        // getter must run every iteration, not just the first.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var count = 0;\n\
                     var o = {};\n\
                     Object.defineProperty(o, 'x', { get: function () { count += 1; return 1; } });\n\
                     function f(o, n) { var s = 0; for (var i = 0; i < n; i++) { s += o.x; } return s; }\n\
                     var r = f(o, 100); r === 100 && count === 100;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_boolean(), Some(true));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_string_concat_in_a_fast_loop() {
        // Cut 41: `s += x` with two strings in a certified loop lowers to
        // `BinLeftReg` whose compiled Add now checks both string tags and
        // calls the rope-concat helper directly — no `binary_slow`.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(x, n) { var s = x; for (var i = 0; i < n; i++) { s += x; } return s.length; }\n\
                     f('ab', 10);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(22.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_string_concat_with_a_number_operand() {
        // Cut 41: only one string operand — the compiled tag check misses
        // and the general Add coerces the number (the fallback stays
        // exact).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(x, n) { var s = x; for (var i = 0; i < n; i++) { s += 1; } return s; }\n\
                     f('x', 3) === 'x111';",
                )
                .expect("runs")
        });
        assert_eq!(value.as_boolean(), Some(true));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_step_path_string_concat() {
        // Cut 41: the step path's `Binary(Add)` (a non-loop body) shares
        // the string-string fast path.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(x) { var s = x + x + x; return s; }\n\
                     f('ab') === 'ababab';",
                )
                .expect("runs")
        });
        assert_eq!(value.as_boolean(), Some(true));
        assert!(compiled >= 1, "{compiled} bodies");
    }
}
