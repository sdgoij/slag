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
//! - Closure creation (`CreateFunction`/`CreateArrow`, the hoisted
//!   `FunctionDeclInit`), `NewTarget`, and `RegExpLiteral`: step-index
//!   helpers read the step's payload back out of the running body and run
//!   the interpreter's instantiation/evaluation machinery against the live
//!   lexical environment. The created closure's own body compiles
//!   separately (the runtime shares one compiled body per declaration
//!   site), so a loop that creates closures now runs entirely in machine
//!   code.
//!
//! Everything else (`with`/`try`/`switch`/`using`, generator suspension,
//! iterator machinery, destructuring/spread, class machinery, global-
//! reference steps, mapped `arguments`) bails to the interpreter.
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
        create_function: Some(rt.create_function),
        create_arrow: Some(rt.create_arrow),
        create_function_decl: Some(rt.create_function_decl),
        new_target: Some(rt.new_target),
        regexp_literal: Some(rt.regexp_literal),
        tail_call: Some(rt.tail_call),
        args_base: Some(rt.args_base),
        args_push: Some(rt.args_push),
        args_spread: Some(rt.args_spread),
        call_vector: Some(rt.call_vector),
        tail_call_vector: Some(rt.tail_call_vector),
        tail_call_self_vector: Some(rt.tail_call_self_vector),
        array_begin: Some(rt.array_begin),
        array_element: Some(rt.array_element),
        array_spread: Some(rt.array_spread),
        array_hole: Some(rt.array_hole),
        array_end: Some(rt.array_end),
        object_begin: Some(rt.object_begin),
        object_init_name: Some(rt.object_init_name),
        object_init_computed: Some(rt.object_init_computed),
        object_key_to_property_key: Some(rt.object_key_to_property_key),
        object_method_name: Some(rt.object_method_name),
        object_method_computed: Some(rt.object_method_computed),
        object_accessor_name: Some(rt.object_accessor_name),
        object_accessor_computed: Some(rt.object_accessor_computed),
        object_spread: Some(rt.object_spread),
        push_str: Some(rt.push_str),
        concat_str: Some(rt.concat_str),
        concat_str_const: Some(rt.concat_str_const),
        push_const: Some(rt.push_const),
        load_const: Some(rt.load_const),
        enter_block: Some(rt.enter_block),
        leave_block: Some(rt.leave_block),
        enter_try: Some(rt.enter_try),
        exit_try: Some(rt.exit_try),
        return_control: Some(rt.return_control),
        break_control: Some(rt.break_control),
        continue_control: Some(rt.continue_control),
        throw_control: Some(rt.throw_control),
        finally_end: Some(rt.finally_end),
        catch_bind: Some(rt.catch_bind),
        dispatch_error: Some(rt.dispatch_error),
        switch_disc: Some(rt.switch_disc),
        switch_test: Some(rt.switch_test),
        for_in_begin: Some(rt.for_in_begin),
        for_in_next: Some(rt.for_in_next),
        for_of_begin: Some(rt.for_of_begin),
        for_of_next: Some(rt.for_of_next),
        for_of_next_bind_local: Some(rt.for_of_next_bind_local),
        for_of_close: Some(rt.for_of_close),
        for_of_close_all: Some(rt.for_of_close_all),
        enter_per_iteration: Some(rt.enter_per_iteration),
        per_iteration: Some(rt.per_iteration),
        yield_suspend: Some(rt.yield_suspend),
        await_suspend: Some(rt.await_suspend),
        destructure_begin: Some(rt.destructure_begin),
        destructure_next: Some(rt.destructure_next),
        destructure_rest: Some(rt.destructure_rest),
        destructure_obj_coercible: Some(rt.destructure_obj_coercible),
        destructure_obj_key: Some(rt.destructure_obj_key),
        destructure_obj_key_computed: Some(rt.destructure_obj_key_computed),
        destructure_obj_key_store: Some(rt.destructure_obj_key_store),
        destructure_obj_key_get: Some(rt.destructure_obj_key_get),
        destructure_obj_rest: Some(rt.destructure_obj_rest),
        destructure_close: Some(rt.destructure_close),
        destructure_obj_end: Some(rt.destructure_obj_end),
        destructure_close_all: Some(rt.destructure_close_all),
        create_arguments: Some(rt.create_arguments),
        typeof_top: Some(rt.typeof_top),
        get_super_base: Some(rt.get_super_base),
        this_value: Some(rt.this_value),
        get_super_name: Some(rt.get_super_name),
        get_super_computed: Some(rt.get_super_computed),
        get_super_computed_keep: Some(rt.get_super_computed_keep),
        assign_super_name: Some(rt.assign_super_name),
        assign_super_computed: Some(rt.assign_super_computed),
        update_super_name: Some(rt.update_super_name),
        update_super_computed: Some(rt.update_super_computed),
        delete_super: Some(rt.delete_super),
        resolve_super_ref_name: Some(rt.resolve_super_ref_name),
        resolve_super_ref_computed: Some(rt.resolve_super_ref_computed),
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
            create_function: Some(helpers::test_create_function),
            create_arrow: Some(helpers::test_create_arrow),
            create_function_decl: Some(helpers::test_create_function_decl),
            new_target: Some(helpers::test_new_target),
            regexp_literal: Some(helpers::test_regexp_literal),
            tail_call: Some(helpers::test_tail_call),
            args_base: Some(helpers::test_args_base),
            args_push: Some(helpers::test_args_push),
            args_spread: Some(helpers::test_args_spread),
            call_vector: Some(helpers::test_call_vector),
            tail_call_vector: Some(helpers::test_tail_call_vector),
            tail_call_self_vector: Some(helpers::test_tail_call_self_vector),
            array_begin: Some(helpers::test_array_begin),
            array_element: Some(helpers::test_array_element),
            array_spread: Some(helpers::test_array_spread),
            array_hole: Some(helpers::test_array_hole),
            array_end: Some(helpers::test_array_end),
            object_begin: Some(helpers::test_object_begin),
            object_init_name: Some(helpers::test_object_init_name),
            object_init_computed: Some(helpers::test_object_init_computed),
            object_key_to_property_key: Some(helpers::test_object_key_to_property_key),
            object_method_name: Some(helpers::test_object_method_name),
            object_method_computed: Some(helpers::test_object_method_computed),
            object_accessor_name: Some(helpers::test_object_accessor_name),
            object_accessor_computed: Some(helpers::test_object_accessor_computed),
            object_spread: Some(helpers::test_object_spread),
            push_str: Some(helpers::test_push_str),
            concat_str: Some(helpers::test_concat_str),
            concat_str_const: Some(helpers::test_concat_str_const),
            push_const: Some(helpers::test_push_const),
            load_const: Some(helpers::test_load_const),
            enter_block: Some(helpers::test_enter_block),
            leave_block: Some(helpers::test_leave_block),
            enter_try: Some(helpers::test_enter_try),
            exit_try: Some(helpers::test_exit_try),
            return_control: Some(helpers::test_return_control),
            break_control: Some(helpers::test_break_control),
            continue_control: Some(helpers::test_continue_control),
            throw_control: Some(helpers::test_throw_control),
            finally_end: Some(helpers::test_finally_end),
            catch_bind: Some(helpers::test_catch_bind),
            dispatch_error: Some(helpers::test_dispatch_error),
            switch_disc: Some(helpers::test_switch_disc),
            switch_test: Some(helpers::test_switch_test),
            for_in_begin: Some(helpers::test_for_in_begin),
            for_in_next: Some(helpers::test_for_in_next),
            for_of_begin: Some(helpers::test_for_of_begin),
            for_of_next: Some(helpers::test_for_of_next),
            for_of_next_bind_local: Some(helpers::test_for_of_next_bind_local),
            for_of_close: Some(helpers::test_for_of_close),
            for_of_close_all: Some(helpers::test_for_of_close_all),
            enter_per_iteration: Some(helpers::test_enter_per_iteration),
            per_iteration: Some(helpers::test_per_iteration),
            yield_suspend: Some(helpers::test_yield_suspend),
            await_suspend: Some(helpers::test_await_suspend),
            destructure_begin: Some(helpers::test_destructure_begin),
            destructure_next: Some(helpers::test_destructure_next),
            destructure_rest: Some(helpers::test_destructure_rest),
            destructure_obj_coercible: Some(helpers::test_destructure_obj_coercible),
            destructure_obj_key: Some(helpers::test_destructure_obj_key),
            destructure_obj_key_computed: Some(helpers::test_destructure_obj_key_computed),
            destructure_obj_key_store: Some(helpers::test_destructure_obj_key_store),
            destructure_obj_key_get: Some(helpers::test_destructure_obj_key_get),
            destructure_obj_rest: Some(helpers::test_destructure_obj_rest),
            destructure_close: Some(helpers::test_destructure_close),
            destructure_obj_end: Some(helpers::test_destructure_obj_end),
            destructure_close_all: Some(helpers::test_destructure_close_all),
            create_arguments: Some(helpers::test_create_arguments),
            typeof_top: Some(helpers::test_typeof_top),
            get_super_base: Some(helpers::test_get_super_base),
            this_value: Some(helpers::test_this_value),
            get_super_name: Some(helpers::test_get_super_name),
            get_super_computed: Some(helpers::test_get_super_computed),
            get_super_computed_keep: Some(helpers::test_get_super_computed_keep),
            assign_super_name: Some(helpers::test_assign_super_name),
            assign_super_computed: Some(helpers::test_assign_super_computed),
            update_super_name: Some(helpers::test_update_super_name),
            update_super_computed: Some(helpers::test_update_super_computed),
            delete_super: Some(helpers::test_delete_super),
            resolve_super_ref_name: Some(helpers::test_resolve_super_ref_name),
            resolve_super_ref_computed: Some(helpers::test_resolve_super_ref_computed),
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
            leaf_call_cache: [runtime::jit::LeafCallSiteCache::empty();
                runtime::jit::LEAF_CALL_CACHE_ENTRIES],
            body: std::ptr::null(),
            tail: false,
            current_function: 0,
            dispatch_value: 0,
            suspension: None,
            suspend_sp: 0,
            resume_kind: 0,
            resume_ip: 0,
            resume_sp: 0,
            resume_value: 0,
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
            vec![
                Step::Push(Value::Number(1.0)),
                Step::EnterWith,
                Step::Return,
            ],
            0,
        );
        assert!(engine.compile(&body, &helpers_all()).is_none());
    }

    #[test]
    fn closure_creation_and_new_target_lower() {
        // Cut 44: closure creation, `new.target`, and RegExp literals are no
        // longer bails — each lowers to a step-index helper call. `NewTarget`
        // needs no payload (the step-index helpers are exercised by the
        // installed e2e tests against the real runtime table).
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(vec![Step::NewTarget, Step::Return], 0);
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_ne!(compiled.info.entry, 0);
    }

    #[test]
    fn tail_call_lowers_and_returns_the_helper_result() {
        // Cut 45: a `TailCallFast` terminates the body with the helper's
        // result (52 from the test double) — no fall-through, no stack leak.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Undefined),
                Step::Push(Value::Number(9.0)),
                Step::Push(Value::Number(1.0)),
                Step::TailCallFast {
                    argc: 1,
                    direct_eval: false,
                },
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(52.0).bits());
    }

    #[test]
    fn tail_call_self_loops_in_machine_code() {
        // Cut 46: a `TailCallSelf` rebinds the frame with the new argument
        // and jumps back to the body's entry — the whole self-recursive
        // chain runs in ONE machine-code invocation. The body decrements
        // slot 0 (a parameter, arity 1) and tail-self-calls until it is 0,
        // then returns it: starting from 5, the loop runs five times with
        // the back edge and returns 0.
        let engine = JitEngine::new().expect("native isa");
        let mut body = make_body(
            vec![
                Step::JumpIfGtImm {
                    slot: 0,
                    imm: 0.0,
                    target: 6,
                },
                Step::LoadLocal { slot: 0 },
                Step::Push(Value::Number(1.0)),
                Step::Binary(syntax::ast::BinaryOp::Sub),
                Step::TailCallSelf { argc: 1 },
                // Unreachable fall-through of the self-call (step 5).
                Step::Push(Value::Undefined),
                Step::LoadLocal { slot: 0 },
                Step::Return,
            ],
            1,
        );
        body.scope = Some(ScopeInfo {
            arity: 1,
            ..scope(1)
        });
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        // The frame is 2 slots (1 + canary); slot 0 starts at 5.
        let mut frame = vec![0u64; 2];
        frame[0] = Value::Number(5.0).bits();
        frame[1] = 0xDEAD_BEEF_CAFE_F00D;
        let mut stack = vec![0u64; 65];
        stack[64] = 0xDEAD_BEEF_CAFE_F00D;
        let mut ctx = runtime::jit::JitCallContext {
            pending: false,
            error: None,
            agent: std::ptr::null_mut(),
            vm: std::ptr::null_mut(),
            global_object: std::ptr::null_mut(),
            global_value_cells: std::ptr::null_mut(),
            member_value_cells: std::ptr::null_mut(),
            clean_chain: false,
            buf_end: std::ptr::null_mut(),
            leaf_epoch: 0,
            leaf_call_cache: [runtime::jit::LeafCallSiteCache::empty();
                runtime::jit::LEAF_CALL_CACHE_ENTRIES],
            body: std::ptr::null(),
            tail: false,
            current_function: 0,
            dispatch_value: 0,
            suspension: None,
            suspend_sp: 0,
            resume_kind: 0,
            resume_ip: 0,
            resume_sp: 0,
            resume_value: 0,
        };
        let result = unsafe {
            compiled.call(
                frame.as_mut_ptr(),
                stack.as_mut_ptr(),
                (&mut ctx as *mut runtime::jit::JitCallContext) as *mut std::os::raw::c_void,
            )
        };
        assert!(!ctx.pending, "a test-double helper set the error flag");
        assert_eq!(frame[1], 0xDEAD_BEEF_CAFE_F00D, "frame overrun");
        assert_eq!(stack[64], 0xDEAD_BEEF_CAFE_F00D, "stack overrun");
        assert_eq!(result, Value::Number(0.0).bits());
    }

    #[test]
    fn vector_call_lowers_and_runs_the_helper() {
        // Cut 49: the vector call form (`ArgsBase`/`ArgsPush` build the
        // argument vector; `Call` runs it) lowers to the helpers — the test
        // doubles return 53 from `call_vector`.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Undefined),
                Step::Push(Value::Number(9.0)),
                Step::ArgsBase,
                Step::Push(Value::Number(1.0)),
                Step::ArgsPush,
                Step::Push(Value::Number(2.0)),
                Step::ArgsPush,
                Step::Call { direct_eval: false },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(53.0).bits());
    }

    #[test]
    fn vector_tail_call_lowers_and_returns_the_helper_result() {
        // Cut 49: the vector `TailCall` terminates the body with the helper's
        // result (54 from the test double).
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Undefined),
                Step::Push(Value::Number(9.0)),
                Step::ArgsBase,
                Step::Push(Value::Number(1.0)),
                Step::ArgsPush,
                Step::TailCall { direct_eval: false },
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(54.0).bits());
    }

    #[test]
    fn array_literal_lowers_through_the_helpers() {
        // Cut 52: the array literal steps lower to the helpers — `ArrayBegin`
        // creates the array (the double returns 60), the element steps echo
        // it back, `ArrayEnd` closes it, and the body returns the value.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::ArrayBegin,
                Step::Push(Value::Number(1.0)),
                Step::ArrayElement,
                Step::ArrayHole,
                Step::Push(Value::Number(2.0)),
                Step::ArrayElement,
                Step::ArrayEnd,
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(60.0).bits());
    }

    #[test]
    fn object_literal_lowers_through_the_helpers() {
        // Cut 53: the object literal steps lower to the helpers — `ObjectBegin`
        // creates the object (the double returns 70), the init/key/spread
        // steps echo it back, and the body returns the value.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::ObjectBegin,
                Step::Push(Value::Number(1.0)),
                Step::ObjectInitName {
                    name: crux::intern_utf8("a"),
                    set_name: false,
                    shorthand: false,
                },
                Step::Push(Value::Number(2.0)),
                Step::ObjectKeyToPropertyKey,
                Step::Push(Value::Number(3.0)),
                Step::ObjectInitComputed { set_name: false },
                Step::Push(Value::Number(4.0)),
                Step::ObjectSpread,
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(70.0).bits());
    }

    #[test]
    fn string_literal_lowers_through_the_helpers() {
        // Cut 54: the string literal steps lower to the helpers — `PushStr`
        // returns the literal (the double returns 80), the concat steps echo
        // the accumulator, and the body returns the value.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::PushStr(crux::JsString::from_utf8("a")),
                Step::PushStr(crux::JsString::from_utf8("b")),
                Step::ConcatStr,
                Step::PushStr(crux::JsString::from_utf8("c")),
                Step::ConcatStrConst(crux::JsString::from_utf8("d")),
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(80.0).bits());
    }

    #[test]
    fn tail_call_self_check_mismatch_runs_the_helper() {
        // Cut 47: a `TailCallSelfCheck` whose resolved callee does NOT match
        // the running closure (`ctx.current_function` is 0) falls to the
        // `tail_call` helper — the test double returns 52.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Undefined),
                Step::Push(Value::Number(5.0)),
                Step::Push(Value::Number(7.0)),
                Step::TailCallSelfCheck { argc: 1 },
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(52.0).bits());
    }

    #[test]
    fn tail_call_self_check_match_loops_in_machine_code() {
        // Cut 47: when the resolved callee IS the running closure
        // (`ctx.current_function`), the check takes the self path — rebind
        // the frame with the new argument and jump back to the body's
        // re-entry — so the whole self-recursive chain runs in ONE
        // machine-code invocation. The body decrements slot 0 (a parameter)
        // and tail-self-calls until 0, returning it: starting from 5, the
        // loop runs five times and returns 0.
        let engine = JitEngine::new().expect("native isa");
        let mut body = make_body(
            vec![
                Step::JumpIfGtImm {
                    slot: 0,
                    imm: 0.0,
                    target: 8,
                },
                Step::Push(Value::Undefined),
                Step::Push(Value::Number(5.0)),
                Step::LoadLocal { slot: 0 },
                Step::Push(Value::Number(1.0)),
                Step::Binary(syntax::ast::BinaryOp::Sub),
                Step::TailCallSelfCheck { argc: 1 },
                // Unreachable fall-through of the check (step 7).
                Step::Push(Value::Undefined),
                Step::LoadLocal { slot: 0 },
                Step::Return,
            ],
            1,
        );
        body.scope = Some(ScopeInfo {
            arity: 1,
            ..scope(1)
        });
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        // The frame is 2 slots (1 + canary); slot 0 starts at 5, and the
        // "running closure" is the constant the body pushes as the callee.
        let mut frame = vec![0u64; 2];
        frame[0] = Value::Number(5.0).bits();
        frame[1] = 0xDEAD_BEEF_CAFE_F00D;
        let mut stack = vec![0u64; 65];
        stack[64] = 0xDEAD_BEEF_CAFE_F00D;
        let mut ctx = runtime::jit::JitCallContext {
            pending: false,
            error: None,
            agent: std::ptr::null_mut(),
            vm: std::ptr::null_mut(),
            global_object: std::ptr::null_mut(),
            global_value_cells: std::ptr::null_mut(),
            member_value_cells: std::ptr::null_mut(),
            clean_chain: false,
            buf_end: std::ptr::null_mut(),
            leaf_epoch: 0,
            leaf_call_cache: [runtime::jit::LeafCallSiteCache::empty();
                runtime::jit::LEAF_CALL_CACHE_ENTRIES],
            body: std::ptr::null(),
            tail: false,
            current_function: Value::Number(5.0).bits(),
            dispatch_value: 0,
            suspension: None,
            suspend_sp: 0,
            resume_kind: 0,
            resume_ip: 0,
            resume_sp: 0,
            resume_value: 0,
        };
        let result = unsafe {
            compiled.call(
                frame.as_mut_ptr(),
                stack.as_mut_ptr(),
                (&mut ctx as *mut runtime::jit::JitCallContext) as *mut std::os::raw::c_void,
            )
        };
        assert!(!ctx.pending, "a test-double helper set the error flag");
        assert_eq!(frame[1], 0xDEAD_BEEF_CAFE_F00D, "frame overrun");
        assert_eq!(stack[64], 0xDEAD_BEEF_CAFE_F00D, "stack overrun");
        assert_eq!(result, Value::Number(0.0).bits());
    }

    #[test]
    fn tail_call_self_vector_loops_in_machine_code() {
        // Cut 51: a `TailCallSelfVector` (a spread/`> FAST_CALL_MAX_ARGS`
        // argument self-call) rebinds the frame from the Vm's argument
        // vector and jumps back to the body's re-entry — the whole
        // self-recursive chain runs in ONE machine-code invocation. The body
        // decrements slot 0 (a parameter, arity 1) into the argument vector
        // and vector-self-calls until it is 0, then returns it: starting
        // from 5, the loop runs five times with the back edge and returns 0.
        // (The test double reports the success signal without touching the
        // frame — the body's own decrement keeps the loop state, and the
        // runtime-side integration verifies the real in-place rebind.)
        let engine = JitEngine::new().expect("native isa");
        let mut body = make_body(
            vec![
                Step::JumpIfGtImm {
                    slot: 0,
                    imm: 0.0,
                    target: 10,
                },
                Step::LoadLocal { slot: 0 },
                Step::Push(Value::Number(1.0)),
                Step::Binary(syntax::ast::BinaryOp::Sub),
                Step::StoreLocal { slot: 0 },
                Step::ArgsBase,
                Step::LoadLocal { slot: 0 },
                Step::ArgsPush,
                Step::TailCallSelfVector,
                // Unreachable fall-through of the self-call (step 9).
                Step::Push(Value::Undefined),
                Step::LoadLocal { slot: 0 },
                Step::Return,
            ],
            1,
        );
        body.scope = Some(ScopeInfo {
            arity: 1,
            ..scope(1)
        });
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        // The frame is 2 slots (1 + canary); slot 0 starts at 5.
        let mut frame = vec![0u64; 2];
        frame[0] = Value::Number(5.0).bits();
        frame[1] = 0xDEAD_BEEF_CAFE_F00D;
        let mut stack = vec![0u64; 65];
        stack[64] = 0xDEAD_BEEF_CAFE_F00D;
        let mut ctx = runtime::jit::JitCallContext {
            pending: false,
            error: None,
            agent: std::ptr::null_mut(),
            vm: std::ptr::null_mut(),
            global_object: std::ptr::null_mut(),
            global_value_cells: std::ptr::null_mut(),
            member_value_cells: std::ptr::null_mut(),
            clean_chain: false,
            buf_end: std::ptr::null_mut(),
            leaf_epoch: 0,
            leaf_call_cache: [runtime::jit::LeafCallSiteCache::empty();
                runtime::jit::LEAF_CALL_CACHE_ENTRIES],
            body: std::ptr::null(),
            tail: false,
            current_function: 0,
            dispatch_value: 0,
            suspension: None,
            suspend_sp: 0,
            resume_kind: 0,
            resume_ip: 0,
            resume_sp: 0,
            resume_value: 0,
        };
        let result = unsafe {
            compiled.call(
                frame.as_mut_ptr(),
                stack.as_mut_ptr(),
                (&mut ctx as *mut runtime::jit::JitCallContext) as *mut std::os::raw::c_void,
            )
        };
        assert!(!ctx.pending, "a test-double helper set the error flag");
        assert_eq!(frame[1], 0xDEAD_BEEF_CAFE_F00D, "frame overrun");
        assert_eq!(stack[64], 0xDEAD_BEEF_CAFE_F00D, "stack overrun");
        assert_eq!(result, Value::Number(0.0).bits());
    }

    #[test]
    fn tail_call_self_check_vector_mismatch_runs_the_vector_helper() {
        // Cut 51: a `TailCallSelfCheckVector` whose resolved callee does NOT
        // match the running closure (`ctx.current_function` is 0) falls to
        // the general vector `tail_call` helper — the test double returns
        // 54.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Undefined),
                Step::Push(Value::Number(5.0)),
                Step::ArgsBase,
                Step::Push(Value::Number(7.0)),
                Step::ArgsPush,
                Step::TailCallSelfCheckVector,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(54.0).bits());
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
            vec![Step::Push(Value::Undefined), Step::EnterWith],
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
    fn call_fast_direct_eval_lowers() {
        // Cut 62: a `CallFast { direct_eval: true }` site now lowers — the
        // slow path threads the flag through to `do_call_fast` (whose
        // `fast_call_core` routes a real `%eval%` callee through
        // `perform_eval` with the caller's environment intact). The
        // compiler never emits one (a direct eval always takes the vector
        // form), so the test proves the step compiles rather than bailing.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![Step::CallFast {
                argc: 0,
                direct_eval: true,
            }],
            0,
        );
        assert!(engine.compile(&body, &helpers_all()).is_some());
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
    fn installed_jit_runs_a_vector_self_tail_call() {
        // Cut 51: a self-tail-call with 10 plain arguments (beyond the fast
        // form's `FAST_CALL_MAX_ARGS` cap) compiles to the vector self-jump
        // — the whole recursive chain runs in ONE machine-code invocation
        // with a bounded native stack.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "\"use strict\"; (function f(n, a, b, c, d, e, g, h, i, j) { \
                     return n ? f(n - 1, a, b, c, d, e, g, h, i, j) : \
                     a + b + c + d + e + g + h + i + j; \
                     }(50000, 1, 2, 3, 4, 5, 6, 7, 8, 9));",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(45.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_spread_self_tail_call() {
        // Cut 52: a spread self-tail-call — the vector form via
        // `ArgsSpread`, with the spread's array literal `[n - 1]` built in
        // machine code (the array-literal steps lower). The whole recursive
        // chain runs in ONE machine-code invocation with a bounded native
        // stack.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "\"use strict\"; (function f(n) { return n ? f(...[n - 1]) : 0; }(50000));",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(0.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_an_array_literal_body() {
        // Cut 52: an array-literal body compiles — `[1, 2, 3]` built in
        // machine code, with holes and a spread.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { return [1, , 3]; } f().length + (function g() { return [1, ...[2, 3]].length; }());",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(6.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_an_object_literal_body() {
        // Cut 53: an object-literal body compiles — plain, computed-key, and
        // spread properties built in machine code.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { var k = 2; return { a: 1, [k]: 3, ...{ d: 4 } }; } \
                     var o = f(); o.a + o[2] + o.d;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(8.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_an_object_literal_with_methods() {
        // Cut 53: method and accessor definitions compile too — the
        // step-index helpers instantiate the functions from the running body.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { return { m() { return 4; }, get g() { return 5; } }; } \
                     var o = f(); o.m() + o.g;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(9.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_string_literal_body() {
        // Cut 54: a body with string literals compiles — `s += 'x'` in a
        // loop (the concat rides the binary Add's `concat_strings` helper).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "\"use strict\"; function f() { var s = ''; \
                     for (var i = 0; i < 5000; i++) { s += 'x'; } return s.length; } f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(5000.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_template_literal_body() {
        // Cut 54: a template literal — `PushStr` + `ConcatStr` +
        // `ConcatStrConst` — compiles.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script("function f(n) { return `a${n}b`.length; } f(7);")
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(3.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_declared_vector_self_tail_call() {
        // Cut 51: the checked vector form — a top-level declaration's own
        // name, 10 plain arguments; the identity check takes the self jump
        // and the whole chain runs in one machine-code invocation.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "\"use strict\"; function f(n, a, b, c, d, e, g, h, i, j) { \
                     return n ? f(n - 1, a, b, c, d, e, g, h, i, j) : \
                     a + b + c + d + e + g + h + i + j; \
                     } f(50000, 1, 2, 3, 4, 5, 6, 7, 8, 9);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(45.0));
        assert!(compiled >= 1, "{compiled} bodies");
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
    fn installed_jit_stale_epoch_leaf_cache_revalidates_at_rest() {
        // Cut 68: a monomorphic hot leaf call next to a disturbing helper
        // (the `o.g` getter bumps the leaf-eligibility epoch on every
        // iteration) must NOT re-probe every iteration. The counting probe
        // wrapper proves it: the first visit warms the cache, and each later
        // visit finds a stale epoch, re-validates that the eligibility state
        // is still at rest, and reuses the cached verdict — the loop's `add`
        // site probes exactly once (a per-iteration re-probe would count
        // ~100K). The upper bound allows the script body's `run()` call site
        // to probe once too, should the script ever compile; today it does
        // not, so the total is 1.
        static PROBE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_leaf_call_probe(
            ctx: *mut c_void,
            callee: u64,
            args: *mut u64,
            argc: u64,
            site: u64,
        ) -> u64 {
            PROBE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.leaf_call_probe)(ctx, callee, args, argc, site)
        }
        let mut helpers = runtime_helpers();
        helpers.leaf_call_probe = Some(counting_leaf_call_probe);

        extern "C" fn noop_drop(_cache: *mut c_void) {}
        let mut agent = runtime::Agent::new();
        agent.initialize_host_defined_realm().expect("realm");
        let mut cache = JitCache::new(helpers).expect("isa");
        agent.jit_hook = Some(runtime::jit::JitHook {
            cache: (&mut cache as *mut JitCache) as *mut c_void,
            lookup: jit_cache_lookup,
            drop_cache: noop_drop,
            helpers: &runtime::jit::JIT_SLOW_PATHS,
        });
        let value = agent
            .run_script(
                "var o = { get g() { return 1; } };\n\
                 function add(a, b) { return a + b; }\n\
                 function run() {\n\
                   var s = 0;\n\
                   for (var i = 0; i < 100000; i++) { s += add(o.g, 1); }\n\
                   return s;\n\
                 }\n\
                 run();",
            )
            .expect("runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value.as_number(), Some(200000.0));
        assert!(compiled >= 2, "{compiled} bodies (run + add) must compile");
        let probes = PROBE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            (1..=2).contains(&probes),
            "the leaf sites must probe once each ({probes} total): a per-iteration re-probe would count ~100K"
        );
    }

    #[test]
    fn installed_jit_two_hot_leaf_sites_each_cache_separately() {
        // Cut 68: TWO hot leaf call sites in one loop body used to alternate
        // on the single `LeafCallSiteCache` record — every visit missed on
        // `site` and re-probed (~200K probes for a 100K-iteration loop). The
        // direct-mapped set (indexed by `site % LEAF_CALL_CACHE_ENTRIES`)
        // gives each site its own record, so each warms once and the loop
        // reuses both verdicts. The counting probe wrapper proves it: 2
        // probes total (one per site), reused for the remaining ~200K
        // visits (an upper bound of 3 allows the script body's `run()` site
        // to probe once too, should the script ever compile).
        static PROBE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_leaf_call_probe(
            ctx: *mut c_void,
            callee: u64,
            args: *mut u64,
            argc: u64,
            site: u64,
        ) -> u64 {
            PROBE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.leaf_call_probe)(ctx, callee, args, argc, site)
        }
        let mut helpers = runtime_helpers();
        helpers.leaf_call_probe = Some(counting_leaf_call_probe);

        extern "C" fn noop_drop(_cache: *mut c_void) {}
        let mut agent = runtime::Agent::new();
        agent.initialize_host_defined_realm().expect("realm");
        let mut cache = JitCache::new(helpers).expect("isa");
        agent.jit_hook = Some(runtime::jit::JitHook {
            cache: (&mut cache as *mut JitCache) as *mut c_void,
            lookup: jit_cache_lookup,
            drop_cache: noop_drop,
            helpers: &runtime::jit::JIT_SLOW_PATHS,
        });
        let value = agent
            .run_script(
                "function add(a, b) { return a + b; }\n\
                 function run() {\n\
                   var s = 0;\n\
                   for (var i = 0; i < 100000; i++) { s += add(1, 1); s += add(2, 2); }\n\
                   return s;\n\
                 }\n\
                 run();",
            )
            .expect("runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value.as_number(), Some(600000.0));
        assert!(compiled >= 2, "{compiled} bodies (run + add) must compile");
        let probes = PROBE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            (2..=3).contains(&probes),
            "each hot site must probe once ({probes} total): alternating single-record misses would count ~200K"
        );
    }

    #[test]
    fn installed_jit_runs_a_loop_creating_closures() {
        // Cut 44: closure creation inside a loop no longer bails the body —
        // `CreateArrow`/`CreateFunction` lower to step-index helpers, so the
        // whole loop runs in machine code. Each iteration adds `g() = i`
        // plus `h(1) = 1 + i`, so `s = sum(2i + 1) = 2*4950 + 100 = 10000`.
        // `f`, the arrow body, and `h`'s body all compile (3 distinct).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(n) { var s = 0; for (var i = 0; i < n; i++) { \
                       var g = () => i; \
                       var h = function (x) { return x + i; }; \
                       s += g() + h(1); \
                     } return s; }\n\
                     f(100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(10000.0));
        assert!(compiled >= 3, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_tail_call_chain() {
        // Cut 47: `function f(n) { return f(n - 1); }` — the callee resolves
        // through the global env, so the machine code identity-checks it
        // against the running closure (`ctx.current_function`) and jumps to
        // the body's re-entry on a match: the 100K-deep chain runs in ONE
        // machine-code invocation, no runtime round-trip.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "\"use strict\";\n\
                     function f(n) { if (n === 0) { return 0; } return f(n - 1); }\n\
                     f(100000);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(0.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_global_name_self_tail_call_rechecks_reassignment() {
        // Cut 47: the checked self-tail-call must NOT jump when the name was
        // reassigned — `f` is now `g`, so f's body tail-calls a different
        // closure and the helper path runs. `original(3)` resolves through
        // f→g→f→g to the replacement's base case (2); a wrongly-taken jump
        // would return the original's (1).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "\"use strict\";\n\
                     function f(n) { if (n === 0) { return 1; } return f(n - 1); }\n\
                     var original = f;\n\
                     f = function g(n) { if (n === 0) { return 2; } return original(n - 1); };\n\
                     original(3);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(2.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_three_arg_call() {
        // Cut 49: a ≥3-argument call compiles through the vector form
        // (`ArgsBase`/`ArgsPush`/`Call`) — the certified script's call to
        // `f(1, 2, 3)` no longer bails the body to the interpreter.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script("function f(a, b, c) { return a + b + c; } f(1, 2, 3);")
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(6.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_spread_call() {
        // Cut 49: `ArgsSpread` iterates the array into the argument vector.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script("function f(a, b, c) { return a + b + c; } f(...[1, 2, 3]);")
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(6.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_vector_tail_call_chain() {
        // Cut 49: a ≥3-argument tail call compiles through the vector
        // `TailCall` — the 100K-deep chain runs with bounded stack.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "\"use strict\";\n\
                     function g(n, a, b, c) { if (n === 0) { return a + b + c; } \
                       return g(n - 1, a + 1, b, c); }\n\
                     g(100000, 0, 1, 2);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(100003.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_self_tail_call_chain() {
        // Cut 46: a named function expression's DIRECT self-tail-call
        // (`return f(n - 1)` inside `function f(n)`) compiles to an
        // in-place frame rebind + jump back to the body's re-entry — the
        // whole 100K-deep chain runs in one machine-code invocation, no
        // runtime round-trip.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "\"use strict\";\n\
                     (function f(n) { if (n === 0) { return 0; } return f(n - 1); }(100000));",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(0.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_tail_call_through_a_closure() {
        // The tco-call-args shape: `getF()(n - 1)` — closure creation plus a
        // computed-callee tail call, both compiled now (`f` and `getF`'s
        // body). `count` lands once at the base case.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "\"use strict\";\n\
                     var count = 0; (function f(n) { if (n === 0) { count += 1; return; } \
                       function getF() { return f; } \
                       return getF()(n - 1); }(100000)); count;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1.0));
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
    fn installed_jit_runs_a_try_catch_body() {
        // Cut 55: a try/catch body compiles — a thrown value dispatches to
        // the catch block in machine code (via `throw_machinery`), the catch
        // parameter binds into its flat slot, and an engine error (a null
        // member read) routes through the same pending-error dispatch.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { try { throw 42; } catch (e) { return e * 2; } }\n\
                     function g() { try { var x = null; return x.y.z; } catch (e) { return e.name; } }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(84.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_try_finally_body() {
        // Cut 55: a return through a finally runs the finally, and a return
        // in the finally overrides the pending return; a break/continue
        // through a finally routes via `control_transfer` too.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var log = [];\n\
                     function f() { try { log.push('t'); return 1; } finally { log.push('f'); } }\n\
                     function g() { try { return 1; } finally { return 2; } }\n\
                     function h() { var out = ''; for (var i = 0; i < 3; i++) { \
                       try { if (i === 1) continue; out += i; } finally { out += 'f'; } } return out; }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_hot_try_catch_loop() {
        // Cut 55: a certified loop whose body contains a try/catch — every
        // even iteration throws and dispatches to the catch in machine code.
        // Sum 0..999 = 499500.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(n) { var s = 0; for (var i = 0; i < n; i++) { \
                       try { if (i % 2 === 0) throw i; s += i; } catch (e) { s += e; } } return s; }\n\
                     f(1000);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(499500.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_nested_try_catch_and_escaping_throw() {
        // Cut 55: nested trys (an inner finally then an outer catch) and a
        // throw that escapes the JIT body into the caller's catch — the
        // escaping value round-trips through the pending error's attached
        // value.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var log = [];\n\
                     function f() { try { try { throw 'inner'; } finally { log.push('f'); } } \
                       catch (e) { return e + '!'; } }\n\
                     function g() { try { throw 'escaped'; } finally { log.push('g'); } }\n\
                     var caught = null;\n\
                     try { g(); } catch (e) { caught = e; }\n\
                     f() + '|' + caught + '|' + log.join(',');",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("inner!|escaped|g,f".to_string())
        );
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_block_env_body() {
        // Cut 55: `EnterBlock`/`LeaveBlock` compile to env push/pop helpers
        // — a nested `let` block now JITs (the block env keeps the env
        // stack balanced for the leaf-probe eligibility checks).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { { let x = 5; var y = x * 2; } return y; }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(10.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_switch_body() {
        // Cut 56: a switch body compiles — the discriminant stores via
        // `switch_disc`, each `SwitchTest` strictly-equals a case test and
        // jumps to the matched case block, and `break` routes through the
        // control dispatch. Includes fall-through, a default in the middle,
        // and a nested switch.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(x) { var out = ''; switch (x) { \
                       case 1: out += 'a'; \
                       case 2: out += 'b'; break; \
                       default: out += 'd'; } return out; }\n\
                     function g(x) { switch (x) { case 1: return 'one'; default: return 'd'; } }\n\
                     function h(x, y) { switch (x) { case 1: switch (y) { case 10: return 'a'; } } return 'z'; }\n\
                     f(1);",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("ab".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_switch_loop_and_try() {
        // Cut 56: a switch in a certified loop with break/continue, and a
        // throw from a switch case caught by an enclosing try (the case
        // bodies' errors route through the Cut 55 handler dispatch).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { var out = ''; for (var i = 0; i < 4; i++) { \
                       switch (i) { case 0: out += 'z'; continue; case 2: break; \
                         default: out += i; } out += '.'; } return out; }\n\
                     function g(x) { try { switch (x) { case 1: throw 'boom'; case 2: return 'two'; } } \
                       catch (e) { return 'caught:' + e; } }\n\
                     f() + '|' + g(1) + '|' + g(2);",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("z1..3.|caught:boom|two".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_hot_switch_loop() {
        // Cut 56: a switch in a certified loop with a break in every case —
        // the matches jump in machine code. Sum over 0..999 of the case
        // values 1/10/100/1000.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(n) { var s = 0; for (var i = 0; i < n; i++) { \
                       switch (i % 4) { case 0: s += 1; break; case 1: s += 10; break; \
                         case 2: s += 100; break; default: s += 1000; } } return s; }\n\
                     f(1000);",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_number(),
            Some(250.0 * (1.0 + 10.0 + 100.0 + 1000.0))
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_for_of_array_sum() {
        // Cut 57: the certified for-of over a plain array takes the fast
        // path (`ForOfBegin` → `ForOfNextBindLocal` fused fetch — the
        // element writes the frame slot directly, no stack round trip). The
        // do-while back edge and the array-literal RHS compile.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { var s = 0; for (var x of [1, 2, 3, 4, 5]) { s += x * 10; } return s; }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(150.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_hot_for_of_array_loop() {
        // Cut 57: a hot for-of over an array literal inside a loop — the
        // per-element fetch + fused bind run in machine code. Sum of
        // 1..100, 100 times.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(n) { var s = 0; for (var i = 0; i < n; i++) { \
                       for (var x of [1, 2, 3, 4]) { s += x; } } return s; }\n\
                     f(100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(100.0 * 10.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_for_of_generic_iterator() {
        // Cut 57: a for-of over a custom `[Symbol.iterator]` takes the
        // generic path — `ForOfBegin` builds the `IteratorRecord` and each
        // `ForOfNext` calls `next()` through the shared `for_of_advance`
        // core (the `for_of_stepping` window around the call).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var iter = {}; iter[Symbol.iterator] = function () { var i = 0; \
                       return { next: function () { return i < 3 ? { value: i++ * 10, done: false } \
                         : { value: undefined, done: true }; } }; };\n\
                     function f() { var s = 0; for (var x of iter) { s += x; } return s; }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(30.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_for_of_string() {
        // Cut 57: a for-of over a string is generic (the String iterator)
        // and yields code points.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { var out = ''; for (var c of 'ab') { out += c + '.'; } return out; }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("a.b.".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_for_of_holes_break_and_continue() {
        // Cut 57: an array hole yields `undefined` (the stock iterator's
        // element Get), and break/continue route through the control
        // dispatch (the for-of boundary keeps the loop's own break from
        // closing early; the loop-bottom fetch's back edge re-runs).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { var out = ''; var a = [1, , 3, 4]; \
                       for (var x of a) { if (x === undefined) { out += 'h'; continue; } \
                         if (x === 4) break; out += x; } return out; }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("1h3".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_for_of_break_and_return_close_the_iterator() {
        // Cut 57: a break to the loop's end runs the compiled `ForOfClose`;
        // a return inside the body routes through `return_control` whose
        // `control_transfer` closes on the escape — both call the iterator's
        // `return` method (spec 14.7.5.6 step 7). The return VALUE
        // expression evaluates before the close, so the log is checked
        // after both calls.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var log = [];\n\
                     var iter = {}; iter[Symbol.iterator] = function () { var i = 0; \
                       return { next: function () { return i < 10 ? { value: i++, done: false } \
                         : { value: undefined, done: true }; }, \
                         return: function () { log.push('c' + i); return {}; } }; };\n\
                     function f() { var s = 0; for (var x of iter) { s += x; if (x === 2) break; } \
                       return s; }\n\
                     function g() { var s = 0; for (var x of iter) { s += x; if (x === 1) \
                       return s; } return -1; }\n\
                     f() + ';' + g() + '|' + log.join(',');",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("3;1|c3,c2".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_for_of_throwing_next_does_not_close() {
        // Cut 57: a `next()` error escapes with the iterator open (spec
        // 14.7.6.2 uses `?` on the next call — only a normal completion or
        // an abrupt body/head completion closes). The `for_of_stepping`
        // flag stays set on the error path, so the engine-error close (the
        // interpreter's Err arm / the JIT's `throw_machinery` escape) skips
        // the iterator's `return`.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var log = [];\n\
                     var iter = {}; iter[Symbol.iterator] = function () { var i = 0; \
                       return { next: function () { if (i++ === 0) return { value: 1, done: false }; \
                         throw 'boom'; }, \
                         return: function () { log.push('closed'); return {}; } }; };\n\
                     function g() { var s = 0; for (var x of iter) { s += x; } return s; }\n\
                     var r = ''; try { g(); } catch (e) { r = e; }\n\
                     r + '|' + log.join(',');",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("boom|".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_for_of_body_error_closes_the_iterator() {
        // Cut 57: an engine error inside the for-of BODY (not the next
        // call) is an abrupt body completion — the iterator must close
        // before the error escapes (spec 14.7.5.6 step 7). The JIT's
        // non-try error path closes the active iterators before surfacing.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var log = [];\n\
                     var boom = {}; Object.defineProperty(boom, 'x', { get: function () { throw 'get'; } });\n\
                     var iter = {}; iter[Symbol.iterator] = function () { var i = 0; \
                       return { next: function () { return i < 3 ? { value: i++, done: false } \
                         : { value: undefined, done: true }; }, \
                         return: function () { log.push('closed'); return {}; } }; };\n\
                     function f() { var s = 0; try { for (var x of iter) { s += boom.x; } } \
                       catch (e) { s += ':e'; } return s + '|' + log.join(','); }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("0:e|closed".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_captured_for_of_head_uses_per_iteration_envs() {
        // Cut 57: a captured lexical for-of head (`for (let x of ...)` with
        // a body closure) emits `EnterPerIteration`/`PerIteration` — the
        // first env pushed at loop entry, a fresh copy per later iteration —
        // and each closure observes its own iteration's binding.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function make() { var fns = []; for (let x of [10, 20, 30]) { \
                       fns.push(function () { return x; }); } return fns.map(function (f) { return f(); }).join(','); }\n\
                     make();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("10,20,30".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_for_in_key_loop() {
        // Cut 57: a certified for-in over an object literal — `ForInBegin`
        // enumerates the keys, each `ForInNext` skips deleted keys and
        // lands the key on the working stack, the `ForOfBindLocal` bind
        // writes it to the head slot.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { var out = ''; for (var k in { a: 1, b: 2, c: 3 }) { out += k; } \
                       return out; }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("abc".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_for_in_prototype_chain_and_nullish_rhs() {
        // Cut 57: for-in walks the prototype chain (the object literal's
        // proto chain via the realm's Object.prototype — an inherited key
        // appears only when enumerable) and a nullish RHS is a skipped loop,
        // not an error.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { var out = ''; var proto = { p: 1 }; var o = Object.create(proto); \
                       o.a = 1; for (var k in o) { out += k; } return out + '|'; }\n\
                     function g() { var out = 'x'; for (var k in null) { out += k; } return out; }\n\
                     f() + g();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("ap|x".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_for_of_in_a_try_body() {
        // Cut 57: a for-of inside a try body — the loop's fetches sit
        // between the try entry and the handler's catch, so a body error
        // routes through the Cut 55 handler dispatch with the for-of state
        // intact (a throwing `return` still closes first).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(n) { var s = 0; try { for (var x of [1, 2, 3, 4]) { \
                       if (x === 3) throw x; s += x; } } catch (e) { s += e * 100; } return s; }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1.0 + 2.0 + 300.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_captured_for_in_head_uses_per_iteration_envs() {
        // Cut 57: a captured lexical for-in head emits the same
        // `EnterPerIteration`/`PerIteration` machinery with `ForInNext` —
        // each closure observes its own iteration's key.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function make() { var fns = []; for (let k in { a: 1, b: 2 }) { \
                       fns.push(function () { return k; }); } \
                       return fns.map(function (f) { return f(); }).join(','); }\n\
                     make();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("a,b".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_an_async_function_with_awaits() {
        // Cut 58: a certified async function compiles — each `await`
        // suspends the machine code (`DISPATCH_SUSPEND`), the driver
        // attaches the promise reactions, and the resume re-enters the
        // compiled body at the continuation with the awaited value pushed.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var result = 'pending';\n\
                     async function f(x) { var a = await x; var b = await (a + 1); return b * 2; }\n\
                     f(10).then(function (v) { result = v; });",
                )
                .expect("runs");
            agent.run_jobs().expect("jobs");
            agent.run_script("result").expect("reads")
        });
        assert_eq!(value.as_number(), Some(22.0));
        // The async body itself compiled (the `await` steps lowered): the
        // body plus the `.then` callback are two distinct compiled bodies.
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_async_function_rejection_routes_through_the_catch() {
        // Cut 58: a rejected `await` resumes with `Resume::Throw`, which the
        // machine code's entry routes through `throw_control` — the
        // machinery finds the body's catch (a static dispatch target) and
        // the resumed segment runs the catch in machine code.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var result = 'pending';\n\
                     async function f() { var log = ''; try { await Promise.reject('boom'); } \
                       catch (e) { log += 'c' + e; } return log + 'done'; }\n\
                     f().then(function (v) { result = v; });",
                )
                .expect("runs");
            agent.run_jobs().expect("jobs");
            agent.run_script("result").expect("reads")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("cboomdone".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_async_function_with_a_finally_and_escaped_rejection() {
        // Cut 58: an `await` inside a try with a finally — the rejected
        // resume routes through the machinery (finally runs, then the throw
        // escapes the body and rejects the promise).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var result = 'pending';\n\
                     async function f() { var log = ''; try { await Promise.reject('x'); } \
                       finally { log += 'f'; } return log; }\n\
                     f().then(function (v) { result = 'ok:' + v; }, function (e) { result = 'rej:' + e; });",
                )
                .expect("runs");
            agent.run_jobs().expect("jobs");
            agent.run_script("result").expect("reads")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("rej:x".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_generator_with_plain_yields() {
        // Cut 58: a certified generator body compiles — each `yield`
        // suspends, `next()` resumes at the continuation, and the final
        // `return` completes the iteration.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function* g() { yield 1; yield 2; return 3; }\n\
                     var out = ''; var it = g(); var r;\n\
                     while (!(r = it.next()).done) { out += r.value + ','; }\n\
                     out += r.value;",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("1,2,3".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_generator_with_a_yield_in_a_fast_loop() {
        // Cut 58: a `yield` inside a certified fast loop — the loop takes
        // the SLOT-counter path (a suspension body never uses the machine-
        // local counter field), so the counter persists in the frame slot
        // across each suspension and the resumed segment continues the loop.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function* g() { for (var i = 0; i < 4; i++) { yield i; } }\n\
                     var out = ''; var it = g(); var r;\n\
                     while (!(r = it.next()).done) { out += r.value + ','; }\n\
                     out += '|' + r.value;",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("0,1,2,3,|undefined".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_generator_throw_routes_through_the_machinery() {
        // Cut 58: `it.throw(v)` at a plain `yield` resumes with
        // `Resume::Throw` — the machine code's entry routes it through
        // `throw_control`, and a body catch catches it; a second `throw`
        // with no catch escapes and completes the generator.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function* g() { var log = ''; try { yield 1; } catch (e) { log += 'c' + e; } \
                       return log + 'r'; }\n\
                     var it = g(); var r1 = it.next(); var r2 = it.throw('boom');\n\
                     r1.value + '|' + r1.done + '|' + r2.value + '|' + r2.done;",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("1|false|cboomr|true".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_async_generator_falls_back_to_the_interpreter() {
        // Cut 58: an async GENERATOR stays on the env path (certification
        // rejects the combined kind) — it must still run correctly.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var result = 'pending';\n\
                     async function* g() { yield 1; yield 2; }\n\
                     (async function () { var out = ''; for await (var v of g()) { out += v; } \
                       return out; })().then(function (v) { result = v; });",
                )
                .expect("runs");
            agent.run_jobs().expect("jobs");
            agent.run_script("result").expect("reads")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("12".to_string())
        );
        // The async generator body is not certified, so it never compiles;
        // the `.then` callback (a plain body) does — proving the hook fired.
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_async_method_returns_nested_async_function() {
        // Cut 58: a strict async METHOD returns an async function capturing
        // the method's param — the inner body resolves it through the
        // closure's [[Environment]] (a strict body's instantiated env would
        // be the Function env — the crash that took down the async-method
        // fixture cluster). The JIT path and the interpreter share the
        // `call_async_function` setup, so this guards both.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var C = class { async method(x) { return async function () { return x; }; } };\n\
                     var c = new C(); var asyncFn = c.method.bind(c); var result = 'pending';\n\
                     asyncFn(7).then(retFn => retFn()).then(v => { result = v; });",
                )
                .expect("runs");
            agent.run_jobs().expect("jobs");
            agent.run_script("result").expect("reads")
        });
        assert_eq!(value.as_number(), Some(7.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_array_destructure_declaration() {
        // Cut 59: a certified body's `let [a, b] = ...` compiles the
        // primitive `Destructure*` steps — the iterator opens via
        // `destructure_begin`, each element via `destructure_next` (landing
        // on the working stack, bound to the frame slots), the close via
        // `destructure_close`.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { let [a, b] = [1, 2]; return a + b * 10; }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(21.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_array_destructure_default_rest_and_generic() {
        // Cut 59: defaults jump through the fixup-patched `DestructureUndef`
        // target, rest collects through `destructure_rest`, and a GENERIC
        // iterator (custom `[Symbol.iterator]`) exercises the same helpers
        // as the dense-array fast path.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() {\n\
                     \x20 let [a, b = 5, ...rest] = [1, undefined, 3, 4];\n\
                     \x20 let [x, y] = { [Symbol.iterator]: function* () { yield 7; yield 8; } };\n\
                     \x20 return JSON.stringify([a, b, rest, x, y]);\n\
                     }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("[1,5,[3,4],7,8]".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_object_destructure_declaration() {
        // Cut 59: `let { x, y: { z } = {}, ...rest } = ...` — constant keys
        // (`DestructureObjKey`), a nested pattern with a default, and the
        // rest copy (`DestructureObjRest` — the static exclusion set read
        // from the step payload).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() {\n\
                     \x20 let { x, y: { z } = {}, ...rest } = { x: 10, y: { z: 20 }, w: 30 };\n\
                     \x20 return JSON.stringify([x, z, rest]);\n\
                     }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("[10,20,{\"w\":30}]".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_destructure_assignment() {
        // Cut 59: assignment destructuring (`[a, b] = v`, `({ p: a, q: b } =
        // v)`) — the elements store to the existing frame slots through
        // `emit_certified_assign_store`.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() {\n\
                     \x20 let a, b;\n\
                     \x20 [a, b] = [7, 8];\n\
                     \x20 ({ p: a, q: b } = { p: 9, q: 11 });\n\
                     \x20 return a * 100 + b;\n\
                     }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(911.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_destructure_in_a_fast_loop() {
        // Cut 59: destructuring inside a certified loop — the pattern steps
        // run per iteration on the hot path.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() {\n\
                     \x20 var sum = 0;\n\
                     \x20 for (var i = 0; i < 3; i++) { let [p, q] = [i, i + 1]; sum += p * 10 + q; }\n\
                     \x20 return sum;\n\
                     }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(36.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_destructure_break_returns_close_the_iterator() {
        // Cut 59: a `break`/`return`/throw ESCAPING a destructuring pattern
        // is impossible (patterns are expressions), but an iterator-`return`
        // must run on a normal `DestructureClose` and the error path must
        // close a mid-pattern iterator. A throwing `next()` leaves it open
        // (the `destructure_stepping` gate).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var closed = 0;\n\
                     var next_calls = 0;\n\
                     function it() {\n\
                     \x20 var n = 0;\n\
                     \x20 return {\n\
                     \x20\x20 [Symbol.iterator]: function () { return this; },\n\
                     \x20\x20 next: function () {\n\
                     \x20\x20\x20 next_calls++;\n\
                     \x20\x20\x20 if (n === 2) throw 'boom';\n\
                     \x20\x20\x20 return { value: n++, done: false };\n\
                     \x20\x20 },\n\
                     \x20\x20 return: function () { closed++; return {}; }\n\
                     \x20 };\n\
                     }\n\
                     var err = '';\n\
                     try { let [a, b, c] = it(); } catch (e) { err = String(e); }\n\
                     err + '|' + closed + '|' + next_calls;",
                )
                .expect("runs")
        });
        // The `next()` error at element 3 escapes with the iterator OPEN
        // (the close machinery skips while `destructure_stepping`): the
        // interpreter's pre-existing behavior, mirrored by the JIT.
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("boom|0|3".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_destructure_closes_iterator_on_completion() {
        // Cut 59: a pattern that consumes FEWER values than the iterator
        // holds closes it on `DestructureClose` (spec 13.15.5.2 step 5) —
        // the iterator's `return` method runs.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var closed = 0;\n\
                     function it() {\n\
                     \x20 var n = 0;\n\
                     \x20 return {\n\
                     \x20\x20 [Symbol.iterator]: function () { return this; },\n\
                     \x20\x20 next: function () { return { value: n++, done: false }; },\n\
                     \x20\x20 return: function () { closed++; return {}; }\n\
                     \x20 };\n\
                     }\n\
                     let [a] = it();\n\
                     closed;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_destructure_captured_names() {
        // Cut 59: a destructured binding captured by a closure — the names
        // allocate capture-context slots and the pattern's `InitContextSlot`
        // binds them.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() {\n\
                     \x20 let [a, b] = [3, 4];\n\
                     \x20 return (function () { return a * 10 + b; })();\n\
                     }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(34.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_strict_unmapped_arguments_returned_object() {
        // Cut 60: a STRICT function created inside a strict script (the
        // `enclosing_strict` forcing) returns its UNMAPPED arguments object.
        // The unmapped form is leaf-eligible, and the JIT leaf runs on a
        // PRIVATE frame buffer while the helper writes `vm.frame` — the
        // regression: a strict `arguments`-returning body returned
        // `undefined` under `--jit` (the Object/defineProperty and
        // arguments-object fixture clusters). `CreateArguments` is now
        // leaf-excluded, so the body runs `run_jit_body` where the frame
        // matches.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "'use strict';\n\
                     var r = (function () { return arguments; })(1, true, 'a');\n\
                     JSON.stringify([r === undefined, typeof r, r.length, r[0], r[2]]);",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("[false,\"object\",3,1,\"a\"]".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_strict_unmapped_arguments_non_leaf_and_descriptor() {
        // Cut 60: the strict unmapped object also works when the body is
        // NOT a leaf (an array literal), and as an object-descriptor value
        // (the `configurable: argObj` shape — ToBoolean of the object).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "'use strict';\n\
                     function g() { var a = [1]; return arguments; }\n\
                     var argObj = g(1, true, 'a');\n\
                     var obj = {};\n\
                     Object.defineProperty(obj, 'p', { configurable: argObj });\n\
                     JSON.stringify([argObj.length, obj.hasOwnProperty('p'), delete obj.p]);",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("[3,true,true]".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_sloppy_mapped_arguments() {
        // Cut 60: a sloppy body observing `arguments` gets the MAPPED object
        // aliasing its simple params through the capture context — reading
        // `arguments[i]` mirrors the param (and `length` is the argument
        // count).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(a, b) { return a + '|' + arguments.length + '|' + arguments[0] + arguments[1]; }\n\
                     f(1, 2);",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("1|2|12".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_mapped_arguments_alias_both_ways() {
        // Cut 60: the mapped object's accessors and the body's own reads
        // share the capture-context bindings — a write through `arguments`
        // is seen by the param and vice versa.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(a) { arguments[0] = 5; return a; }\n\
                     function g(a) { a = 7; return arguments[0]; }\n\
                     f(1) + '|' + g(1);",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("5|7".to_string())
        );
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_arguments_callee_and_strict_unmapped() {
        // Cut 60: `arguments.callee` resolves through the running context's
        // function; a STRICT body gets the UNMAPPED object (a param write is
        // not reflected).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { return typeof arguments.callee; }\n\
                     function g(a) { 'use strict'; a = 9; return arguments[0]; }\n\
                     f() + '|' + g(1);",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("function|1".to_string())
        );
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_mapped_arguments_with_captured_params() {
        // Cut 60: a closure inside the body captures a param (context slot)
        // while `arguments` aliases it — the mapped object and the closure
        // observe the same capture-context binding.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(a) {\n\
                     \x20 var g = function () { return a; };\n\
                     \x20 arguments[0] = 11;\n\
                     \x20 return g() + '|' + arguments[0];\n\
                     }\n\
                     f(1);",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("11|11".to_string())
        );
        assert!(compiled >= 1, "{compiled} bodies");
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
    fn installed_jit_script_completion_matches_the_interpreter() {
        // Cut 65: a certified script now runs through the JIT; its
        // fall-off-end completion must match the interpreter's. The compiled
        // completion steps write `vm.completion`/`completion_is_empty` (the
        // register the interpreter's fall-off-end arm reads), and the script
        // path converts the machine code's past-the-end value to that
        // register's completion. Each case also asserts the script body (and
        // its leaf callee) actually compiled.
        let cases: &[(&str, Option<f64>, usize)] = &[
            // The top-level bench shapes.
            (
                "var s = 0; for (var i = 0; i < 100; i++) { s += i; } s;",
                Some(4950.0),
                1,
            ),
            (
                "function g(x) { return x + 1; } var t = 0; for (var i = 0; i < 100; i++) { t += g(i); } t;",
                Some(5050.0),
                2,
            ),
            (
                "function g(x) { return x + 1; } var s = 0; for (var i = 0; i < 100; i++) { s = g(i); } s;",
                Some(100.0),
                2,
            ),
            // A var declaration produces no completion value.
            ("var x = 1;", None, 1),
            // A statement-position assignment carries its value
            // (`FusedStoreLocal` sets the completion).
            ("var x; x = 5;", Some(5.0), 1),
            // Control statements: with and without a value.
            ("if (true) { 3 }", Some(3.0), 1),
            ("if (false) { 3 }", None, 1),
            // A trailing block that ends empty restores the pre-block
            // completion (`5; { var q = 1; }` completes 5, not undefined).
            ("5; { var q = 1; }", Some(5.0), 1),
            // ...but a control statement inside the block (ResetCompletion +
            // NormalizeCompletion) turns the register empty, so the block's
            // empty end does not restore 5.
            ("5; { if (true) {} }", None, 1),
            // The fused call-store only fires for plain slot args; a literal
            // arg keeps the `FusedStoreLocal` tail, which sets the completion.
            (
                "function g(x) { return x + 1; } var s = 0; s = g(1);",
                Some(2.0),
                2,
            ),
            // In the counter path the loop body's `FusedStoreLocal` sets the
            // completion on the last iteration (the last `s = g(i)` = 3).
            (
                "function g(x) { return x + 1; } var s = 0; for (var i = 0; i < 3; i++) { s = g(i); }",
                Some(3.0),
                2,
            ),
        ];
        for (source, expected, min_compiled) in cases {
            let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("runs"));
            match expected {
                Some(n) => assert_eq!(value.as_number(), Some(*n), "{source}"),
                None => assert_eq!(value, Value::Undefined, "{source}"),
            }
            assert!(compiled >= *min_compiled, "{source}: {compiled} bodies");
        }
    }

    #[test]
    fn installed_jit_runs_a_fused_slot_call_store() {
        // Cut 65: the param-callee twin — `s = g(i)` fuses into
        // `CallFastSlotStore` (the callee comes from the frame slot). The
        // fused step now LOWERs: `f`'s body compiles (the arg slots
        // materialize with their TDZ checks, the call runs through the leaf
        // probe, the result stores to the target), so the loop runs in
        // machine code with the anonymous callee's certified-leaf body
        // in-frame.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(g, n) { var s = 0; for (var i = 0; i < n; i++) { s = g(i); } return s; }\n\
                     f(function (x) { return x + 1; }, 100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(100.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_fused_global_call_store() {
        // Cut 65: a certified SCRIPT's `s = g(i)` fuses into
        // `CallFastGlobalStore` (the never-assigned global `g` is the
        // statically-known callee). The step now lowers — the top-level
        // loop's arg loads, the global-cell callee read, the leaf-probe
        // call, and the store all run in machine code (previously the
        // script bailed to the interpreter).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function g(x) { return x + 1; }\n\
                     var s = 0;\n\
                     for (var i = 0; i < 100; i++) { s = g(i); }\n\
                     s;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(100.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_fused_global_call() {
        // Cut 65: the non-store form — a certified script's expression-
        // position `g(i)` fuses into `CallFastGlobal` (callee from the
        // global fast cell, `undefined` receiver). The whole loop runs in
        // machine code.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function g(x) { return x + 1; }\n\
                     var t = 0;\n\
                     for (var i = 0; i < 100; i++) { t += g(i); }\n\
                     t;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(5050.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_tail_call_to_a_leaf() {
        // `return g(x)` in tail position: in sloppy mode the compiler emits a
        // normal call (TCO is strict-only), so f's body compiles with the
        // fused slot call and g (a certified leaf) runs in-frame via the
        // leaf probe. The driver is called 1000 times, so the leaf path
        // fires a thousand JIT runs.
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

    #[test]
    fn installed_jit_super_method_call_keeps_this() {
        // Cut 61: `super.m()` lowers to `ThisValue` + `GetSuperBase` +
        // `GetSuperName` + `CallFast` — the receiver must be the current
        // this (the base method reads `this.v`), not the base object the
        // `GetSuperBase` capture left on the stack. The derived
        // constructor itself is env-path (bailed); the method bodies
        // compile.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "class A { m() { return this.v; } }\n\
                     class B extends A { constructor() { super(); this.v = 42; } m() { return super.m(); } }\n\
                     new B().m();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(42.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_super_computed_read_and_call() {
        // Cut 61: the computed shapes — `super[k]` (read: `GetSuperBase` +
        // key + `GetSuperComputed`) and `super[j]()` (call: `ThisValue` +
        // `GetSuperBase` + key + `GetSuperComputed` + `CallFast`). The two
        // stack shapes differ by the extra this-value push; a height mixup
        // would land the wrong receiver. The read key hits the prototype
        // accessor; the call key the prototype method.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "class A { constructor() { this._x = 40; } get x() { return this._x; } m() { return 41; } }\n\
                     class B extends A { m(k, j) { var a = super[k]; var b = super[j](); return a + b; } }\n\
                     new B().m('x', 'm');",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(81.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_super_assign_and_compound() {
        // Cut 61: `super.x = v` (`GetSuperBase` + `AssignSuperName`) and
        // `super.x += v` (the compound: base + `Dup` + `GetSuperName` old +
        // `AssignSuperName { op }`) — both write through the super
        // reference with the current this as the receiver. The accessor
        // lives on the BASE (the read/write must go through the prototype
        // chain, not the instance's own property).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "class A { constructor() { this._x = 10; } get x() { return this._x; } set x(v) { this._x = v; } }\n\
                     class B extends A { m() { super.x = 5; super.x += 3; return this._x; } }\n\
                     new B().m();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(8.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_super_update_prefix_postfix() {
        // Cut 61: `super.x++`/`--` route through the pre-resolved
        // reference (`ResolveSuperRefName`/`ResolveSuperRefComputed` +
        // `GetVarReference` + `UpdateVarReference`). Sequence: postfix ++
        // (10→11), prefix ++ (11→12), postfix ++ computed (12→13), prefix
        // -- computed (13→12).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "class A { constructor() { this._x = 10; } get x() { return this._x; } set x(v) { this._x = v; } }\n\
                     class B extends A { m(k) { var a = super.x++; var b = ++super.x; var c = super[k]++; var d = --super[k]; return a * 1000 + b * 100 + c * 10 + d; } }\n\
                     new B().m('x');",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(11332.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_super_logical_assign() {
        // Cut 61: `super.x &&= v` / `super.x ??= v` — the `ResolveSuperRef*`
        // + `GetVarReference` + `PutVarReference` chain with both the write
        // path (old truthy/nullish) and the short-circuit path (old keeps
        // the expression result). `super[k] &&= 3` exercises the computed
        // resolve.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "class A { constructor() { this._x = 10; this._y = null; } get x() { return this._x; } set x(v) { this._x = v; } get y() { return this._y; } set y(v) { this._y = v; } }\n\
                     class B extends A { m(k) { super.x &&= 5; super.y ??= 7; super[k] &&= 3; var sx = super.x; var sy = super.y; super.x ??= 99; var sh = super.x; return sx * 1000 + sy * 100 + sh; } }\n\
                     new B().m('x');",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(3703.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_super_in_async_method() {
        // Cut 61: a certified async method body using `super` — the async
        // driver must set `current_function` for the whole run, or the
        // `vm_this_binding`/`vm_super_base` reads fail (no home object /
        // this slot on the certified path).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "class A { m() { return 41; } }\n\
                     var B = class extends A { async m() { return super.m() + 1; } };\n\
                     var result = 'pending';\n\
                     new B().m().then(v => { result = v; });",
                )
                .expect("runs");
            agent.run_jobs().expect("jobs");
            agent.run_script("result").expect("reads")
        });
        assert_eq!(value.as_number(), Some(42.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_delete_super_is_reference_error() {
        // Cut 61: `delete super.x` / `delete super[k]` is a ReferenceError
        // before the key is evaluated (spec 13.5.1.2 step 4.b) — both the
        // name and computed forms surface it through the pending-error path.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "class A {} class B extends A { m() { var hits = 0; try { delete super.x; } catch (e1) { hits += (e1 instanceof ReferenceError ? 1 : 0); } try { delete super['x']; } catch (e2) { hits += (e2 instanceof ReferenceError ? 1 : 0); } return hits; } }\n\
                     new B().m();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(2.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_super_static_and_inherited_base() {
        // Cut 61: the base is the home object's prototype — for a static
        // method that is the superclass CONSTRUCTOR (B.[[Prototype]] = A),
        // and for an inherited method the receiver's class differs from the
        // method's home object (C inherits `n` from B, whose home is
        // B.prototype).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "class A { static s() { return 41; } static v = 7; }\n\
                     class B extends A { static m() { return super.s() + super.v; } }\n\
                     class D { n() { return 10; } } class E extends D { n() { return super.n() * 2; } } class F extends E {}\n\
                     B.m() + new F().n();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(68.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_super_read_and_call_stack_shapes() {
        // Cut 61: `super.getX` (read: `[base] -> [value]`) and `super.getX()`
        // (call: `[this, base] -> [this, value]` + `CallFast`) compile to
        // different working-stack heights; a mixed-up shape lands the wrong
        // this or base. One body exercises both plus a member read of a
        // script-global (`proto.getX` via the env-path reference
        // machinery).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var proto = { getX: function () { return 41; } };\n\
                     var o = { __proto__: proto, m() { var f = super.getX; return (f === proto.getX && super.getX() === 41) ? 42 : 0; } };\n\
                     o.m();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(42.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_new_target_normal_call_is_undefined() {
        // Cut 62: `new.target` now certifies — a plain function's body
        // compiles the `NewTarget` step (the general path; the step stays
        // leaf-excluded). A normal call's per-run `current_new_target` is
        // unset, so the read is `undefined`.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script("function f() { return new.target; } f() === undefined;")
                .expect("runs")
        });
        assert_eq!(value.as_boolean(), Some(true));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_new_target_in_construct_is_the_constructor() {
        // Cut 62: the certified construct path sets `current_new_target` —
        // `new F()` (a base constructor with a certified body) reads its
        // own `new.target`. The construct body runs via the certified
        // construct path (interpreter), so the value assertion is the
        // proof; the method body (`m`) compiles through the JIT.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function F() { this.t = new.target; }\n\
                     class C { m() { return new.target; } }\n\
                     (new F().t === F) && (new C().m() === undefined);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_boolean(), Some(true));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_heap_string_constant_in_a_loop() {
        // Cut 62: a string literal (`Push(Value::String)`) now embeds its
        // NaN-boxed pointer bits directly instead of a `push_const` helper
        // call — the loop's `s += 'x'` concat stays exact across 10
        // iterations (a stale/dangling constant would surface as a wrong
        // length or a crash).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(n) { var s = ''; for (var i = 0; i < n; i++) { s += 'x'; } return s.length; }\n\
                     f(10);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(10.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_heap_bigint_constant_leaf() {
        // Cut 62: a bigint literal (`Push(Value::BigInt)`) embeds its bits
        // too, and this body is a LEAF (Push + Return — the embedded
        // constant rides the leaf path's private frame/working buffers).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script("function f() { return 10n; } f() === 10n;")
                .expect("runs")
        });
        assert_eq!(value.as_boolean(), Some(true));
        assert!(compiled >= 1, "{compiled} bodies");
    }
}
