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
pub mod opt;
pub mod opt_lower;

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
/// bodies are evicted. Sized for a whole application's body set — a scene with
/// more bodies than the cap recompiles its LRU tail (a small body compiles in
/// ~0.4ms, which is a frame's budget), and the entries are bounded machine code
/// rather than heap objects, so the cap is generous on purpose.
pub const MAX_CACHE_ENTRIES: usize = 1024;

/// Eviction removes entries down to this floor (half the capacity), so a
/// burst of new bodies does not thrash the cache entry by entry.
pub const EVICT_TO_ENTRIES: usize = 512;

impl JitCache {
    /// A cache whose compile step uses `helpers` as the slow-path table.
    pub fn new(helpers: JitHelpers) -> Result<Self, String> {
        Self::with_capacity(helpers, MAX_CACHE_ENTRIES, EVICT_TO_ENTRIES)
    }

    /// A cache whose engine forces the optimizing-tier lowering on — the I1
    /// equivalence tests (the default `new` takes it from `SLAG_OPT`).
    #[cfg(test)]
    pub(crate) fn new_with_opt(helpers: JitHelpers) -> Result<Self, String> {
        let mut cache = Self::new(helpers)?;
        cache.engine = JitEngine::with_opt(true)?;
        Ok(cache)
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
                // reconsult the cache (and recompile). The eviction count and
                // the reset consult counter are what keep that recompile from
                // repeating every call — see `CompiledBody::jit_evictions`.
                entry.body.jit_info.set(0);
                entry.body.jit_calls.set(0);
                entry
                    .body
                    .jit_evictions
                    .set(entry.body.jit_evictions.get().saturating_add(1));
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
        unary_slow: Some(rt.unary_slow),
        concat_strings: Some(rt.concat_strings),
        builder_bind: Some(rt.builder_bind),
        builder_store: Some(rt.builder_store),
        builder_append: Some(rt.builder_append),
        relational_slow: Some(rt.relational_slow),
        update_value_slow: Some(rt.update_value_slow),
        to_boolean_slow: Some(rt.to_boolean_slow),
        tdz_error: Some(rt.tdz_error),
        gc_safepoint: Some(rt.gc_safepoint),
        get_member_name: Some(rt.get_member_name),
        get_member_map_slot: Some(rt.get_member_map_slot),
        get_member_computed: Some(rt.get_member_computed),
        set_member_name: Some(rt.set_member_name),
        set_member_computed: Some(rt.set_member_computed),
        rmw_compound_computed: Some(rt.rmw_compound_computed),
        rmw_update_computed: Some(rt.rmw_update_computed),
        call_slow: Some(rt.call_slow),
        leaf_call_probe: Some(rt.leaf_call_probe),
        leaf_call_fill: Some(rt.leaf_call_fill),
        leaf_call_env: Some(rt.leaf_call_env),
        certified_call: Some(rt.certified_call),
        get_global: Some(rt.get_global),
        set_global: Some(rt.set_global),
        set_global_slot: Some(rt.set_global_slot),
        load_ident: Some(rt.load_ident),
        typeof_ident: Some(rt.typeof_ident),
        resolve_var_ident: Some(rt.resolve_var_ident),
        put_var_reference: Some(rt.put_var_reference),
        update_ident: Some(rt.update_ident),
        assign_member_name: Some(rt.assign_member_name),
        assign_member_computed: Some(rt.assign_member_computed),
        fast_array_element_write: Some(rt.fast_array_element_write),
        dense_array_append: Some(rt.dense_array_append),
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
        construct: Some(rt.construct),
        tagged_template: Some(rt.tagged_template),
        call_apply: Some(rt.call_apply),
        apply_args_fill: Some(rt.apply_args_fill),
        tail_call_vector: Some(rt.tail_call_vector),
        tail_call_self_vector: Some(rt.tail_call_self_vector),
        array_begin: Some(rt.array_begin),
        array_element: Some(rt.array_element),
        array_spread: Some(rt.array_spread),
        array_hole: Some(rt.array_hole),
        array_end: Some(rt.array_end),
        array_fast: Some(rt.array_fast),
        object_begin: Some(rt.object_begin),
        object_fast: Some(rt.object_fast),
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
        for_of_fast_next: Some(rt.for_of_fast_next),
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
        char_code_at: Some(rt.char_code_at),
        array_index_of: Some(rt.array_index_of),
        map_get: Some(rt.map_get),
        set_has: Some(rt.set_has),
        map_set: Some(rt.map_set),
        array_at: Some(rt.array_at),
        array_includes: Some(rt.array_includes),
        array_push: Some(rt.array_push),
        typed_array_length: Some(rt.typed_array_length),
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

/// Print the temporary helper-call instrument's histogram (`JIT_HELPER_STATS`),
/// one line per helper that ran: `helper <index> <count>`. The index is
/// `Helper as usize`; `crates/jit/src/helpers.rs` defines the order. A no-op
/// when the instrument was not enabled at compile time (the counters stay zero).
pub fn dump_helper_counts() {
    if std::env::var("JIT_HELPER_STATS").is_err() {
        return;
    }
    let counts = runtime::jit::JIT_HELPER_COUNTS.snapshot();
    for (index, count) in counts.iter().enumerate() {
        if *count > 0 {
            println!("helper\t{index}\t{count}");
        }
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
    use runtime::ir::{ApplyKind, CompiledBody, ScopeInfo, Step};
    use std::sync::atomic::Ordering;

    /// A certified-style scope for the hand-built test bodies: 2 slots, both
    /// `var`-like (no TDZ), nothing captured.
    fn scope(frame_size: usize) -> ScopeInfo {
        ScopeInfo {
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

    fn make_body(steps: Vec<Step>, frame_size: usize) -> CompiledBody {
        let max_stack = runtime::ir::max_stack_usage(&steps);
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
            jit_calls: std::cell::Cell::new(0),
            jit_evictions: std::cell::Cell::new(0),
            ident_names: Vec::new(),
            has_loop: false,
            has_call_apply: false,
            has_call_intrinsic: false,
            max_stack,
            nested_gate: std::cell::Cell::new(None),
            feedback: std::cell::RefCell::new(None),
        }
    }

    fn helpers_all() -> JitHelpers {
        JitHelpers {
            binary_slow: Some(helpers::test_binary_slow),
            unary_slow: Some(helpers::test_unary_slow),
            concat_strings: Some(helpers::test_concat_strings),
            builder_bind: Some(helpers::test_builder_bind),
            builder_store: Some(helpers::test_builder_store),
            builder_append: Some(helpers::test_builder_append),
            relational_slow: Some(helpers::test_relational_slow),
            update_value_slow: Some(helpers::test_update_value_slow),
            to_boolean_slow: Some(helpers::test_to_boolean_slow),
            tdz_error: Some(helpers::test_tdz_error),
            gc_safepoint: Some(helpers::test_gc_safepoint),
            get_member_name: Some(helpers::test_get_member_name),
            get_member_map_slot: Some(helpers::test_get_member_map_slot),
            get_member_computed: Some(helpers::test_get_member_computed),
            set_member_name: Some(helpers::test_set_member_name),
            set_member_computed: Some(helpers::test_set_member_computed),
            rmw_compound_computed: Some(helpers::test_rmw_compound_computed),
            rmw_update_computed: Some(helpers::test_rmw_update_computed),
            call_slow: Some(helpers::test_call_slow),
            leaf_call_probe: Some(helpers::test_leaf_call_probe),
            leaf_call_fill: Some(helpers::test_leaf_call_fill),
            leaf_call_env: Some(helpers::test_leaf_call_env),
            certified_call: Some(helpers::test_certified_call),
            get_global: Some(helpers::test_get_global),
            set_global: Some(helpers::test_set_global),
            set_global_slot: Some(helpers::test_set_global_slot),
            load_ident: Some(helpers::test_load_ident),
            typeof_ident: Some(helpers::test_typeof_ident),
            resolve_var_ident: Some(helpers::test_resolve_var_ident),
            put_var_reference: Some(helpers::test_put_var_reference),
            update_ident: Some(helpers::test_update_ident),
            assign_member_name: Some(helpers::test_assign_member_name),
            assign_member_computed: Some(helpers::test_assign_member_computed),
            fast_array_element_write: Some(helpers::test_fast_array_element_write),
            dense_array_append: Some(helpers::test_dense_array_append),
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
            construct: Some(helpers::test_construct),
            tagged_template: Some(helpers::test_tagged_template),
            call_apply: Some(helpers::test_call_apply),
            apply_args_fill: Some(helpers::test_apply_args_fill),
            tail_call_vector: Some(helpers::test_tail_call_vector),
            tail_call_self_vector: Some(helpers::test_tail_call_self_vector),
            array_begin: Some(helpers::test_array_begin),
            array_element: Some(helpers::test_array_element),
            array_spread: Some(helpers::test_array_spread),
            array_hole: Some(helpers::test_array_hole),
            array_end: Some(helpers::test_array_end),
            array_fast: Some(helpers::test_array_fast),
            object_begin: Some(helpers::test_object_begin),
            object_fast: Some(helpers::test_object_fast),
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
            for_of_fast_next: Some(helpers::test_for_of_fast_next),
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
            char_code_at: Some(helpers::test_char_code_at),
            array_index_of: Some(helpers::test_array_index_of),
            map_get: Some(helpers::test_map_get),
            set_has: Some(helpers::test_set_has),
            map_set: Some(helpers::test_map_set),
            array_at: Some(helpers::test_array_at),
            array_includes: Some(helpers::test_array_includes),
            array_push: Some(helpers::test_array_push),
            typed_array_length: Some(helpers::test_typed_array_length),
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

    /// A leaf-call record table for a test context: the compiled record gate
    /// reads it (the test doubles never write it), so a leaked table per test
    /// keeps a compiled body with a call step from dereferencing a null base.
    fn test_leaf_records() -> *mut runtime::jit::LeafCallRecord {
        Box::leak(Box::new(
            [runtime::jit::LeafCallRecord::empty(); runtime::jit::LEAF_CALL_RECORDS],
        ))
        .as_mut_ptr()
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
            global_bits: 0,
            global_value_cells: std::ptr::null_mut(),
            member_value_cells: std::ptr::null_mut(),
            computed_read_cells: std::ptr::null_mut(),
            member_map_cells: std::ptr::null_mut(),
            globals_unshadowed: false,
            buf_end: std::ptr::null_mut(),
            leaf_epoch: 0,
            leaf_records: test_leaf_records(),
            leaf_gen: 0,
            body: std::ptr::null(),
            tail: false,
            current_function: 0,
            self_entry: 0,
            self_stack_usage: 0,
            self_inline_ok: false,
            apply_builtin_bits: 0,
            call_builtin_bits: 0,
            intrinsic_bits: [0; runtime::ir::INTRINSIC_COUNT],
            dispatch_value: 0,
            suspension: None,
            suspend_sp: 0,
            resume_kind: 0,
            resume_ip: 0,
            resume_sp: 0,
            resume_value: 0,
            gc_ticks: runtime::jit::JIT_GC_PROBE_INTERVAL,
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
                    num: None,
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
                    limit: runtime::ir::RelLimit::Imm(1000.0),
                    inc: syntax::ast::UpdateOp::Increment,
                    body_start: 7,
                    after: 12,
                },
                Step::FastLoopStore {
                    var: runtime::ir::FastLoopVar::Slot(0),
                    num: None,
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
                    num: None,
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
                    limit: runtime::ir::RelLimit::Imm(10.0),
                    inc: syntax::ast::UpdateOp::Increment,
                    body_start: 5,
                    after: 7,
                },
                Step::FastLoopStore {
                    var: runtime::ir::FastLoopVar::Slot(0),
                    num: None,
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
    fn slow_unary_uses_the_helper() {
        // The coercing kinds (`+x`, `-x`, `~x`) route through `unary_slow`,
        // whose test double returns -42.
        let engine = JitEngine::new().expect("native isa");
        for op in [
            syntax::ast::UnaryOp::Plus,
            syntax::ast::UnaryOp::Minus,
            syntax::ast::UnaryOp::BitNot,
        ] {
            let body = make_body(
                vec![
                    Step::Push(Value::Number(1.0)),
                    Step::Unary(op),
                    Step::Return,
                ],
                0,
            );
            let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
            assert_eq!(run(&compiled, 0), Value::Number(-42.0).bits(), "{op:?}");
        }
    }

    #[test]
    fn coercing_unary_bails_without_the_helper() {
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Unary(syntax::ast::UnaryOp::Minus),
                Step::Return,
            ],
            0,
        );
        assert!(engine.compile(&body, &helpers_none()).is_none());
    }

    #[test]
    fn not_matches_the_interpreter() {
        // `!x` is a truthiness test plus a select: every non-heap operand is
        // covered by `emit_truthiness`'s inline path (a Number or one of the
        // falsy/truthy tags), so only the Boolean tags and the bare frame
        // move — a heap operand would take the `to_boolean_slow` double.
        let engine = JitEngine::new().expect("native isa");
        for (value, expected) in [
            (Value::Undefined, true),
            (Value::Null, true),
            (Value::Boolean(false), true),
            (Value::Number(0.0), true),
            (Value::Number(f64::NAN), true),
            (Value::Boolean(true), false),
            (Value::Number(1.0), false),
        ] {
            let body = make_body(
                vec![
                    Step::Push(value),
                    Step::Unary(syntax::ast::UnaryOp::Not),
                    Step::Return,
                ],
                0,
            );
            let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
            assert_eq!(
                run(&compiled, 0),
                Value::Boolean(expected).bits(),
                "expected !operand == {expected}"
            );
        }
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
            global_bits: 0,
            global_value_cells: std::ptr::null_mut(),
            member_value_cells: std::ptr::null_mut(),
            computed_read_cells: std::ptr::null_mut(),
            member_map_cells: std::ptr::null_mut(),
            globals_unshadowed: false,
            buf_end: std::ptr::null_mut(),
            leaf_epoch: 0,
            leaf_records: test_leaf_records(),
            leaf_gen: 0,
            body: std::ptr::null(),
            tail: false,
            current_function: 0,
            self_entry: 0,
            self_stack_usage: 0,
            self_inline_ok: false,
            apply_builtin_bits: 0,
            call_builtin_bits: 0,
            intrinsic_bits: [0; runtime::ir::INTRINSIC_COUNT],
            dispatch_value: 0,
            suspension: None,
            suspend_sp: 0,
            resume_kind: 0,
            resume_ip: 0,
            resume_sp: 0,
            resume_value: 0,
            gc_ticks: runtime::jit::JIT_GC_PROBE_INTERVAL,
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
                Step::Call {
                    direct_eval: false,
                    span: crux::Span::new(0, 0),
                },
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
    fn apply_call_step_lowers_and_runs_the_helper() {
        // The compiled `Step::CallApply` (.notes/perf.md "remaining apply floor")
        // lowers to the `call_apply` helper: `[f, apply/call, thisArg,
        // a1..aN]` on the work stack, the argument region passed by pointer.
        // The test double sums the argument region (the `thisArg` first), so
        // `[1, 2]` returns 3.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Number(7.0)), // f
                Step::Push(Value::Number(8.0)), // the resolved apply
                Step::Push(Value::Number(1.0)), // thisArg
                Step::Push(Value::Number(2.0)), // argArray
                Step::CallApply {
                    argc: 2,
                    kind: ApplyKind::Apply,
                    span: crux::Span::new(0, 0),
                },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(3.0).bits());
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
    fn object_fast_lowers_to_the_fused_helper() {
        // Cut 72: a whole-simple literal lowers to ONE `ObjectFast` step —
        // the fused helper (the double returns 70) reads the values below
        // the working sp, the machine code drops them, and the body returns
        // the created value. Values pushed v0 then v1; ObjectFast pops 2
        // and pushes the object.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Push(Value::Number(2.0)),
                Step::ObjectFast {
                    names: Box::new([crux::intern_utf8("a"), crux::intern_utf8("b")]),
                },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(70.0).bits());
    }

    #[test]
    fn array_fast_lowers_to_the_fused_helper() {
        // The array analogue of `object_fast_lowers_to_the_fused_helper`: a
        // whole-simple literal lowers to ONE `ArrayFast` step — the fused
        // helper (the double returns 61) reads the values below the working
        // sp, the machine code drops them, and the body returns the created
        // value. Values pushed v0 then v1; ArrayFast pops 2 and pushes the
        // array.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Push(Value::Number(2.0)),
                Step::ArrayFast { count: 2 },
                Step::Return,
            ],
            0,
        );
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        assert_eq!(run(&compiled, 0), Value::Number(61.0).bits());
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
            global_bits: 0,
            global_value_cells: std::ptr::null_mut(),
            member_value_cells: std::ptr::null_mut(),
            computed_read_cells: std::ptr::null_mut(),
            member_map_cells: std::ptr::null_mut(),
            globals_unshadowed: false,
            buf_end: std::ptr::null_mut(),
            leaf_epoch: 0,
            leaf_records: test_leaf_records(),
            leaf_gen: 0,
            body: std::ptr::null(),
            tail: false,
            current_function: Value::Number(5.0).bits(),
            self_entry: 0,
            self_stack_usage: 0,
            self_inline_ok: false,
            apply_builtin_bits: 0,
            call_builtin_bits: 0,
            intrinsic_bits: [0; runtime::ir::INTRINSIC_COUNT],
            dispatch_value: 0,
            suspension: None,
            suspend_sp: 0,
            resume_kind: 0,
            resume_ip: 0,
            resume_sp: 0,
            resume_value: 0,
            gc_ticks: runtime::jit::JIT_GC_PROBE_INTERVAL,
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
            global_bits: 0,
            global_value_cells: std::ptr::null_mut(),
            member_value_cells: std::ptr::null_mut(),
            computed_read_cells: std::ptr::null_mut(),
            member_map_cells: std::ptr::null_mut(),
            globals_unshadowed: false,
            buf_end: std::ptr::null_mut(),
            leaf_epoch: 0,
            leaf_records: test_leaf_records(),
            leaf_gen: 0,
            body: std::ptr::null(),
            tail: false,
            current_function: 0,
            self_entry: 0,
            self_stack_usage: 0,
            self_inline_ok: false,
            apply_builtin_bits: 0,
            call_builtin_bits: 0,
            intrinsic_bits: [0; runtime::ir::INTRINSIC_COUNT],
            dispatch_value: 0,
            suspension: None,
            suspend_sp: 0,
            resume_kind: 0,
            resume_ip: 0,
            resume_sp: 0,
            resume_value: 0,
            gc_ticks: runtime::jit::JIT_GC_PROBE_INTERVAL,
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
        // ...and the eviction is counted, with the consult count restarted, so
        // the runtime's lookup gate makes B wait for the threshold again
        // instead of recompiling it on its next call.
        assert_eq!(b.jit_evictions.get(), 1);
        assert_eq!(b.jit_calls.get(), 0);
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
    fn cache_keeps_an_application_sized_working_set_compiled() {
        // An application's body set routinely runs to a few hundred bodies
        // (a scene plus its mods). All of them stay compiled: evicting the LRU
        // tail would make every evicted body recompile per frame.
        let mut cache = JitCache::new(helpers_all()).expect("isa");
        let bodies: Vec<std::rc::Rc<CompiledBody>> = (0..300)
            .map(|i| {
                std::rc::Rc::new(make_body(
                    vec![Step::Push(Value::Number(i as f64)), Step::Return],
                    0,
                ))
            })
            .collect();
        for body in &bodies {
            assert!(!cache.lookup(body, false).is_null());
        }
        assert_eq!(cache.compiled_count(), 300);
        assert!(
            bodies.iter().all(|body| body.jit_evictions.get() == 0),
            "no body of a 300-body working set is evicted"
        );
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
                    span: crux::Span::new(0, 0),
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
                span: crux::Span::new(0, 0),
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

    /// Like [`with_jit_agent`] but with the optimizing tier forced on
    /// (`SLAG_OPT`), so the I1 lowering is exercised end to end.
    fn with_opt_jit_agent(f: impl FnOnce(&mut runtime::Agent) -> Value) -> (Value, usize) {
        extern "C" fn noop_drop(_cache: *mut c_void) {}
        let mut agent = runtime::Agent::new();
        agent.initialize_host_defined_realm().expect("realm");
        let mut cache = JitCache::new_with_opt(runtime_helpers()).expect("isa");
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
    fn opt_tier_straight_line_body_matches_the_interpreter() {
        // I1: a straight-line certified function (params + `var`s + arithmetic)
        // is lifted to the SSA IR and lowered through `opt_lower`, and must
        // agree with the interpreter. The outer script body has calls, so the
        // lift refuses it and it stays on the per-step path — the test proves
        // both the equivalence and that `f` itself took the optimizing path.
        let source = "function f(a, b) { \
                        var x = a + b; \
                        var y = x * 2 - b; \
                        return (y + a) / 2; \
                      } \
                      var t = 0; \
                      t += f(1, 2); t += f(2, 3); t += f(3, 4); t += f(4, 5); \
                      t += f(5, 6); t += f(6, 7); t += f(7, 8); t += f(8, 9); \
                      t += f(9, 1); t += f(1, 9); t += f(2, 8); t += f(3, 7); \
                      t += f(4, 6); t += f(5, 5); t += f(6, 4); t += f(7, 3); \
                      t += f(8, 2); t += f(9, 0); t += f(0, 9); t += f(1, 1); \
                      t;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let before = crate::opt_lower::OPT_COMPILED.load(Ordering::Relaxed);
        let (value, compiled) = with_opt_jit_agent(|agent| agent.run_script(source).expect("runs"));
        let lowered = crate::opt_lower::OPT_COMPILED.load(Ordering::Relaxed) - before;
        assert_eq!(
            value, interp,
            "the optimizing tier must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies compiled");
        assert!(lowered >= 1, "the optimizing tier lowered {lowered} bodies");
    }

    #[test]
    fn opt_tier_branch_body_matches_the_interpreter() {
        // I2: a body with `if`/`else` and a ternary lowers to the multi-block
        // IR (a branch plus a join whose stack value becomes a parameter) and
        // must agree with the interpreter.
        let source = "function f(a, b) { \
                        var r; \
                        if (a > b) { r = a - b; } else { r = b - a; } \
                        return r > 3 ? r * 2 : r + 100; \
                      } \
                      var t = 0; \
                      t += f(1, 2); t += f(9, 4); t += f(3, 3); t += f(7, 0); \
                      t += f(5, 6); t += f(2, 8); t += f(4, 1); t += f(0, 9); \
                      t += f(8, 8); t += f(6, 2); t += f(1, 7); t += f(9, 9); \
                      t += f(3, 5); t += f(7, 1); t += f(2, 2); t += f(4, 9); \
                      t += f(0, 0); t += f(8, 3); t += f(6, 6); t += f(5, 1); \
                      t += f(9, 2); t += f(1, 4); t += f(7, 7); t += f(3, 0); \
                      t;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let before = crate::opt_lower::OPT_COMPILED.load(Ordering::Relaxed);
        let (value, compiled) = with_opt_jit_agent(|agent| agent.run_script(source).expect("runs"));
        let lowered = crate::opt_lower::OPT_COMPILED.load(Ordering::Relaxed) - before;
        assert_eq!(
            value, interp,
            "the optimizing tier must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies compiled");
        assert!(lowered >= 1, "the optimizing tier lowered {lowered} bodies");
    }

    #[test]
    fn opt_tier_loop_body_matches_the_interpreter() {
        // I2b: a `do`/`while` loop takes the plain back-edge path (its body
        // starts at step 0, so the back edge targets the entry block and the
        // lowering's prologue is exercised) and must agree with the
        // interpreter.
        let source = "function f(n) { var s = 0; var i = n; \
                        do { s = s + i; i = i - 1; } while (i > 0); \
                        return s; } \
                      var t = 0; \
                      t += f(3); t += f(5); t += f(1); t += f(7); \
                      t += f(2); t += f(9); t += f(4); t += f(6); \
                      t += f(0); t += f(8); t += f(10); t += f(11); \
                      t += f(12); t += f(13); t += f(14); t += f(15); \
                      t += f(16); t += f(17); t += f(18); t += f(19); \
                      t;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let before = crate::opt_lower::OPT_COMPILED.load(Ordering::Relaxed);
        let (value, compiled) = with_opt_jit_agent(|agent| agent.run_script(source).expect("runs"));
        let lowered = crate::opt_lower::OPT_COMPILED.load(Ordering::Relaxed) - before;
        assert_eq!(
            value, interp,
            "the optimizing tier must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies compiled");
        assert!(lowered >= 1, "the optimizing tier lowered {lowered} bodies");
    }

    #[test]
    fn opt_tier_folded_constants_match_the_interpreter() {
        // The pass pipeline runs between the lift and the lowering: `(1 + 2) *
        // 3 - 4` lifts to a chain of `Const`s and arithmetic ops, folds to a
        // single `Const`, and DCE drops the dead operands. The body must still
        // match the interpreter, and the fold counter proves the pipeline
        // actually fired (the compiler leaves literal arithmetic as steps).
        let source = "function f() { return (1 + 2) * 3 - 4; } \
                      var t = 0; \
                      t += f(); t += f(); t += f(); t += f(); t += f(); \
                      t += f(); t += f(); t += f(); t += f(); t += f(); \
                      t += f(); t += f(); t += f(); t += f(); t += f(); \
                      t += f(); t += f(); t += f(); t += f(); t += f(); \
                      t;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let before = crate::opt::pass::fold::FOLDS.load(Ordering::Relaxed);
        let (value, compiled) = with_opt_jit_agent(|agent| agent.run_script(source).expect("runs"));
        let folds = crate::opt::pass::fold::FOLDS.load(Ordering::Relaxed) - before;
        assert_eq!(
            value, interp,
            "the optimizing tier must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies compiled");
        assert!(folds >= 1, "the pipeline folded {folds} constants");
    }

    #[test]
    fn installed_jit_runs_a_member_callee() {
        // `return o.f(1) + 1` — a member callee (plain `CallFast`), no loop.
        // Cut 69: both bodies are straight-line, so the call repeats 17×
        // against the ONE hoisted callee site (the completion stays the
        // last call's value).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(o) { return o.f(1) + 1; } \
                     var o = { f: function (x) { return x + 1; } }; \
                     f(o); f(o); f(o); f(o); f(o); f(o); \
                     f(o); f(o); f(o); f(o); f(o); f(o); \
                     f(o); f(o); f(o); f(o); f(o);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(3.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_known_number_operands_match_the_interpreter() {
        // Slice A of the type-specialized loop lowering: the register
        // executor's accumulator provenance lets an op whose operands are
        // already canonical Numbers skip its tag checks (and its slow
        // block entirely when BOTH are known) — the `i * 2` of a numeric
        // reduction lowers to a bare `fmul`. The string-concat arm guards
        // the boundary: a Number accumulator combined with a String constant
        // must still take the slow path (right operand not known-Number), so
        // `i + '!'` concatenates rather than adding.
        let source = "function bench() {\n\
                        var n = 0;\n\
                        for (var i = 0; i < 100000; i++) { n += i * 2; }\n\
                        return n;\n\
                      }\n\
                      function cat() {\n\
                        var out = '';\n\
                        for (var i = 0; i < 3; i++) { out = i + '!'; }\n\
                        return out;\n\
                      }\n\
                      var a = bench();\n\
                      var b = cat();\n\
                      (a === 9999900000 && b === '2!') ? 1 : 0;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "known-Number operand lowering must match the interpreter"
        );
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_loop_carried_number_slots_match_the_interpreter() {
        // Slice B (Route A): a frame slot whose only in-loop reference is one
        // arithmetic RMW (`n += i * 2`, `n -= 1`) is kept in an f64 register
        // across its canonical loop and flushed to the frame at the loop exit.
        // The guarded shapes must stay generic and still agree: a slot whose
        // entry write is conditional (`if (true) { n = 1; }` — a branch in the
        // prefix) and a non-Number init (`n = 'x'`, whose `+=` concatenates).
        let source = "function reduce() {\n\
                        var n = 0;\n\
                        for (var i = 0; i < 100000; i++) { n += i * 2; }\n\
                        return n;\n\
                      }\n\
                      function dec() {\n\
                        var n = 5;\n\
                        for (var i = 0; i < 10; i++) { n -= 1; }\n\
                        return n;\n\
                      }\n\
                      function conditional() {\n\
                        var n = 0;\n\
                        if (true) { n = 1; }\n\
                        for (var i = 0; i < 3; i++) { n += i; }\n\
                        return n;\n\
                      }\n\
                      function concat() {\n\
                        var n = 'x';\n\
                        for (var i = 0; i < 3; i++) { n += i; }\n\
                        return n;\n\
                      }\n\
                      [reduce(), dec()];\n\
                      (reduce() === 9999900000 && dec() === -5 &&\n\
                       conditional() === 4 && concat() === 'x012') ? 1 : 0;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "loop-carried Number slot lowering must match the interpreter"
        );
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_deferred_number_accumulator_matches_the_interpreter() {
        // Slice C: the register executor keeps a known Number accumulator in an
        // f64 and materializes the `Value`-bits form only when a consumer needs
        // it. Exercise the consumers — a leaf body's `ReturnAcc`, a computed
        // member store (the accumulator as the stored value), and arithmetic
        // across several ops — against the interpreter.
        let source = "function callLeaf(fn) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < 2000; i++) { s += fn(i); }\n\
                        return s;\n\
                      }\n\
                      function store(o, k) {\n\
                        var n = 0;\n\
                        for (var i = 0; i < 100; i++) { n += i * 3; o[k] = n; }\n\
                        return o[k];\n\
                      }\n\
                      var a = callLeaf(function (x) { return x * 2 + 1; });\n\
                      var o = {};\n\
                      var b = store(o, 'v');\n\
                      (a === 4000000 && b === 14850) ? 1 : 0;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "deferred accumulator materialization must match the interpreter"
        );
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_indirect_call_into_an_object_literal_body() {
        // Cut 72: the compiled `ObjectFast` helper reads its `names` payload
        // back from the RUNNING body (`string_literal_step_reads_body`), so a
        // body containing one must not leaf-inline — an inlined `object_fast`
        // read the CALLER's body and tripped its unreachable!. The indirect
        // `f()` call site resolves to the one literal-plus-loop callee.
        let source = "function make() {\n\
\t                        var o = { a: 1, b: 2 };\n\
\t                        var n = 0;\n\
\t                        for (var i = 0; i < 2000; i++) { n += o.a + o.b; }\n\
\t                        return n;\n\
\t                      }\n\
\t                      function via(f) { var r = 0; for (var k = 0; k < 3; k++) { r = f(); } return r; }\n\
\t                      via(make);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "an indirect call into an object-literal body must match the interpreter"
        );
        assert_eq!(value.as_number(), Some(6000.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_hoists_invariant_member_reads_without_changing_semantics() {
        // LICM: a data-property read on a loop-invariant frame-slot receiver
        // is hoisted to a hidden slot; an accessor, an object-valued operand,
        // and a member write in the body must all keep per-iteration
        // semantics (the guard misses or the loop is not hoisted).
        let source = "function data() { var o = { a: 1, b: 2 }; var n = 0; for (var i = 0; i < 500; i++) { n += o.a + o.b; } return n; }\n\
\t                      function accessor() { var o = { b: 2 }; var c = 0; Object.defineProperty(o, 'a', { get: function () { return ++c; } }); var n = 0; for (var i = 0; i < 4; i++) { n += o.a + o.b; } return n * 100 + c; }\n\
\t                      function objectOperand() { var o = { a: 1 }; var n = { valueOf: function () { return 1; } }; for (var i = 0; i < 3; i++) { n = n + o.a; } return n; }\n\
\t                      function mutated() { var o = { a: 1 }; var n = 0; for (var i = 0; i < 3; i++) { n += o.a; o.a = o.a + 1; } return n; }\n\
\t                      function boolRhs() { var o = { a: 1, b: 2 }; var n = 0; for (var i = 0; i < 3; i++) { n += (o.a < o.b); } return n; }\n\
\t                      function strRhs() { var o = { s: 'x', t: 'y' }; var r = ''; for (var i = 0; i < 4; i++) { r += o.s + o.t; } return r; }\n\
\t                      (data() === 1500 && accessor() === 1804 && objectOperand() === 4 && mutated() === 6 && boolRhs() === 3 && strRhs() === 'xyxyxyxy') ? 1 : 0;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the member-read hoist must match the interpreter"
        );
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_hoists_invariant_global_reads_without_changing_semantics() {
        // LICM globals: a loop reading a name that resolves at the global env
        // is hoisted to a hidden slot when the cell's data value is a
        // primitive and the body cannot write it. Every soundness edge must
        // keep per-iteration semantics: a SHADOWING env (a closure created
        // inside a `with`, whose chain is not the global env — the
        // `globals_unshadowed` gate must miss so the shadowed value is read), an
        // object-valued global (per-iteration ToPrimitive), a global written
        // in the loop, an accessor global (the cell never warms), and a
        // counter-dependent RHS (only the read hoists).
        //
        // Order matters: any global write bumps the global object's
        // generation and invalidates every value cell, so `mut`/`getter`
        // (which write globals) come last — before them the `gNum` cell is
        // warm, which is exactly what makes the shadowed read discriminating
        // (a hoist that ignored `globals_unshadowed` would read the global's 5).
        let source = "var gNum = 5; var gStr = 'x'; var gObj = { valueOf: function () { return 2; } }; var gMut = 1; var gAcc = 0;\n\
                      Object.defineProperty(globalThis, 'gGetter', { get: function () { return ++gAcc; }, configurable: true });\n\
                      function warm(n) { var s = 0; for (var i = 0; i < n; i++) { s += gNum; } return s; }\n\
                      function num() { var s = 0; for (var i = 0; i < 5; i++) { s += gNum; } return s; }\n\
                      function str() { var r = ''; for (var i = 0; i < 4; i++) { r += gStr; } return r; }\n\
                      function obj() { var s = 0; for (var i = 0; i < 3; i++) { s += gObj; } return s; }\n\
                      function mut() { var s = 0; for (var i = 0; i < 3; i++) { s += gMut; gMut = gMut + 1; } return s * 100 + gMut; }\n\
                      function getter() { var s = 0; for (var i = 0; i < 4; i++) { s += gGetter; } return s * 10 + gAcc; }\n\
                      function counter() { var s = 0; for (var i = 0; i < 4; i++) { s += gNum + i; } return s; }\n\
                      var shadowed;\n\
                      with ({ gNum: 100 }) { shadowed = function (n) { var s = 0; for (var i = 0; i < n; i++) { s += gNum; } return s; }; }\n\
                      warm(1);\n\
                      var ok = num() === 25 && str() === 'xxxx' && obj() === 6 && shadowed(3) === 300\n\
                        && counter() === 26 && mut() === 604 && getter() === 104;\n\
                      ok ? 1 : 0;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the global-read hoist must match the interpreter"
        );
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_registered_builtin_calls_match_the_interpreter() {
        // Cut 79: a member call whose callee is an installed builtin
        // (Map.prototype.has/get/set — registered at `Intrinsics::define`)
        // runs the native handler directly from `fast_call_core` in both
        // engines, skipping the per-call %eval% intrinsic lookup and the
        // `call_inner` redispatch. The compiled loop must agree with the
        // interpreter on the churn result, the wrong-receiver TypeError
        // (the handler throws it), and a receiver whose own property
        // shadows the builtin method (no fast path — the shadow runs).
        let source = "function f(m) { var s = 0; for (var i = 0; i < 20000; i++) { var k = i & 63; if (m.has(k)) { s += m.get(k); } m.set(k, i & 255); } return s; }\n\
                     var m = new Map();\n\
                     for (var j = 0; j < 64; j++) { m.set(j, j); }\n\
                     var base = f(m);\n\
                     var threw = '';\n\
                     try { Map.prototype.has.call({}, 'a'); } catch (e) { threw = e.name; }\n\
                     var shadowed = '';\n\
                     var m2 = new Map(); m2.set(1, 2);\n\
                     m2.has = function () { return 'shadow'; };\n\
                     shadowed = m2.has(1);\n\
                     if (threw !== 'TypeError' || shadowed !== 'shadow') { throw 'fastpath-mismatch'; }\n\
                     base;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the compiled registered-builtin calls must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_primitive_proto_reads_match_the_interpreter() {
        // The primitive-prototype own-data read cache (get_member_name's
        // `primitive_proto_data_get`): a primitive method read (`s.charAt`)
        // is served from the member-value cell keyed by %String.prototype%
        // once warm. The cell is validated against the prototype's
        // generation, so every mutation must be observed by the next read:
        // an overwrite mid-loop, an accessor conversion, a redefinition of
        // a non-writable data property, and a delete. The interpreter and
        // the compiled path (whose primitive reads call_slow into the same
        // machinery) must agree on all of them.
        let source = "function churn() {\n\
                     var s = \"abcdefghij\";\n\
                     var acc = [];\n\
                     for (var i = 0; i < 4; i++) {\n\
                       acc.push(s.charAt(0));\n\
                       if (i === 1) { String.prototype.charAt = function () { return \"X\"; }; }\n\
                     }\n\
                     acc.push(s.charAt(0));\n\
                     String.prototype.charAt = function (n) { return \"Y\" + n; };\n\
                     acc.push(s.charAt(0));\n\
                     return acc.join(\"|\");\n\
                   }\n\
                   var orig = String.prototype.charAt;\n\
                   var a = churn();\n\
                   delete String.prototype.charAt;\n\
                   var b = typeof \"\".charAt;\n\
                   String.prototype.charAt = orig;\n\
                   var c = churn();\n\
                   Object.defineProperty(String.prototype, \"charAt\", {\n\
                     get: function () { return function () { return \"G\"; }; },\n\
                     configurable: true\n\
                   });\n\
                   var d = \"x\".charAt(0);\n\
                   delete String.prototype.charAt;\n\
                   var e = typeof \"x\".charAt;\n\
                   String.prototype.charAt = orig;\n\
                   Object.defineProperty(String.prototype, \"ro\", {\n\
                     value: 1, writable: false, enumerable: false, configurable: true\n\
                   });\n\
                   var f = \"x\".ro;\n\
                   Object.defineProperty(String.prototype, \"ro\", {\n\
                     value: 2, writable: false, enumerable: false, configurable: true\n\
                   });\n\
                   var g = \"x\".ro;\n\
                   delete String.prototype.ro;\n\
                   [a, b, c, d, e, f, g].join(\"|\");";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the compiled primitive-prototype reads must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_native_builtin_calls_match_the_interpreter() {
        // The crux-native fast path (Math/JSON/typed-array methods whose
        // memoized dispatch verdict is "no module chain applies"): the
        // compiled loop's `Math.floor`/`Math.abs` calls run the native
        // closure directly from `fast_call_core`. Must agree with the
        // interpreter on the churn result, and a direct `eval` inside a
        // compiled function (never memoized — the %eval% identity check
        // catches it before any dispatch resolution) must still evaluate.
        let source = "function f(n) { var s = 0; for (var i = 0; i < n; i++) { s += Math.floor(i * 0.5); s += Math.abs(i - 1000); } return s; }\n\
                     function g() { return eval('1 + 2'); }\n\
                     f(20000) + g();";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the compiled crux-native builtin calls must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_registered_builtin_constructs_match_the_interpreter() {
        // The construct-side fast path (Cut 83): a compiled loop's
        // `new Object()` / `new Map()` / `new Date()` run the registered
        // builtin construct handler directly from `step_construct_impl`,
        // skipping the `dispatch_construct` chain walk. Must agree with the
        // interpreter on the churn result, and a subclassed `new` (callee =
        // the EcmaScript subclass, not the registered builtin) must still
        // construct through the ordinary path.
        let source = "function f(n) { var s = 0; for (var i = 0; i < n; i++) { var o = new Object(); var m = new Map(); m.set('k', i); var d = new Date(i); var a = new Array(i); var ac = Array(i); if (m.get('k') === i && d.getTime() === i && a.length === i && ac.length === i && Object.getPrototypeOf(a) === Array.prototype && Object.getPrototypeOf(ac) === Array.prototype) { s++; } } return s; }\n\
                     function g() { class M extends Map {} var m = new M(); m.set(1, 2); return m.get(1) === 2 && m instanceof M ? 1 : 0; }\n\
                     function h() { class A extends Array {} var a = new A(3); var r = Reflect.construct(Array, [3], A); return (a instanceof A && Object.getPrototypeOf(a) === A.prototype && Array.isArray(a) && r.length === 3 && Object.getPrototypeOf(r) === A.prototype) ? 1 : 0; }\n\
                     f(20000) + g() + h();";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the compiled registered-builtin constructs must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_shape_read_serves_cycling_same_shape_objects() {
        // Slice 1: the compiled `GetMemberName` read falls back to an
        // inline shape read when the (id, name) value cell misses — probe
        // the shared (map id, name) cells and read the receiver's
        // `in_fields` slot at the recorded offset. A 64-object cycling loop
        // thrashes the 16-entry value cells every pass, so every read would
        // otherwise call the `get_member_name` helper; the shape path must
        // serve the same values with no per-object identity (a map id pins
        // the descriptor layout for every instance of the shape).
        let source = "function C(v) { this.a = v; this.b = v + 1; }\n\
                     var os = [];\n\
                     for (var i = 0; i < 64; i++) { os.push(new C(i)); }\n\
                     var s = 0;\n\
                     for (var r = 0; r < 200; r++) { for (var j = 0; j < 64; j++) { s += os[j].b; } }\n\
                     s;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(value, interp, "the shape read must match the interpreter");
        assert_eq!(value.as_number(), Some(416000.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_shape_read_serves_cycling_overflow_fields() {
        // Slice 3: a map-described key at an ordinal >= INLINE_FIELDS (a
        // six-field shape's `.f`) has no machine-addressable inline storage,
        // so the shape gate routes it through the narrow map-slot helper
        // (the machine validated the (map id, name) cells first). A
        // 64-object cycling loop thrashes the value cells every pass; the
        // narrow helper must serve the same values as the interpreter.
        let source = "function C(v) { this.a = v; this.b = v + 1; this.c = v + 2; this.d = v + 3; this.e = v + 4; this.f = v + 5; }\n\
                     var os = [];\n\
                     for (var i = 0; i < 64; i++) { os.push(new C(i)); }\n\
                     var s = 0;\n\
                     for (var r = 0; r < 200; r++) { for (var j = 0; j < 64; j++) { s += os[j].f; } }\n\
                     s;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the overflow shape read must match the interpreter"
        );
        assert_eq!(
            value.as_number(),
            Some(200.0 * (64.0 * 63.0 / 2.0 + 5.0 * 64.0))
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_shape_store_serves_cycling_same_shape_objects() {
        // Slice 2: the compiled plain member store falls back to a shape
        // gate when the (id, name) value cell misses — probe the shared
        // (map id, name) map cells and route a map-described key to the
        // narrow `set_member_slot` write. A 64-object cycling store loop
        // thrashes the 16-entry value cells every pass, so every store would
        // otherwise call the full assign helper; the shape gate must land
        // every value on its own instance (the writable check stays in the
        // helper).
        let source = "function C(v) { this.a = v; this.b = v + 1; }\n\
                     var os = [];\n\
                     for (var i = 0; i < 64; i++) { os.push(new C(i)); }\n\
                     for (var r = 0; r < 200; r++) { for (var j = 0; j < 64; j++) { os[j].b = r * 2; } }\n\
                     var s = 0;\n\
                     for (var j = 0; j < 64; j++) { s += os[j].b; }\n\
                     s;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(value, interp, "the shape store must match the interpreter");
        assert_eq!(value.as_number(), Some(64.0 * 398.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_vector_free_field_update_matches_the_interpreter() {
        // The compiled vector-free field UPDATE: a written, writable
        // map-pinned in-object field on a deferred receiver is stored inline.
        // An own writable data property shadows the whole chain (spec
        // 7.3.3), so a prototype setter must NOT intercept the update of an
        // already-written own field — the case the hole-fill's chain gate
        // declines. The result must match the interpreter exactly (a
        // non-writable boilerplate field like a function's `length` declines
        // to the helper; the shape cell for it is only warm once read).
        let source = "var hits = 0;\n\
                      function C() { this.x = 1; }\n\
                      var o = new C();\n\
                      Object.defineProperty(Object.prototype, 'x', { set: function (v) { hits += v; }, configurable: true });\n\
                      var f = function (p, q) {};\n\
                      function hot(a, fn) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < 200; i++) { a.x = i; s += a.x; }\n\
                        for (var i = 0; i < 200; i++) { s += fn.length; fn.length = i; }\n\
                        return s;\n\
                      }\n\
                      var acc = 0;\n\
                      for (var c = 0; c < 40; c++) { acc += hot(o, f); }\n\
                      acc + hits * 100000 + o.x * 10 + f.length;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(value, interp, "the field update must match the interpreter");
        assert_eq!(value.as_number(), Some(40.0 * 20300.0 + 199.0 * 10.0 + 2.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_constructor_fill_defers_to_a_mid_run_prototype_setter() {
        // The compiled vector-free constructor fill (a map-described presize
        // hole written inline) must stay exact against the prototype chain:
        // once a hot constructor's fills have warmed the compiled path, a
        // setter installed on the prototype must intercept the NEXT store
        // ([[Set]] consults the chain while the field is still a hole). The
        // map cell's chain-clean provenance records the receiver's direct
        // prototype (id, generation); the defineProperty bumps it and
        // declines the inline fill to the exact helper. Without the gate the
        // machine fill would define an own property and bypass the setter.
        let source = "var hits = 0;\n\
                     function C() { this.x = 1; }\n\
                     var s = 0;\n\
                     for (var i = 0; i < 40; i++) { var o = new C(); s += o.x; }\n\
                     Object.defineProperty(C.prototype, 'x', {\n\
                       set: function (v) { hits += v; },\n\
                       configurable: true\n\
                     });\n\
                     var o2 = new C();\n\
                     s * 100000 + hits * 10 + (Object.prototype.hasOwnProperty.call(o2, 'x') ? 1 : 0);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the compiled fill must match the interpreter"
        );
        assert_eq!(value.as_number(), Some(40.0 * 100000.0 + 1.0 * 10.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_unary_operators_match_the_interpreter() {
        // The coercing kinds (`+x`, `-x`, `~x`) route through the real
        // `unary_slow` (the interpreter's `eval_unary_value`); `!x` lowers
        // inline. Covered: the inline number path, a numeric-string `+`, a
        // BigInt `-`/`~`, a `!` over a heap value (the `to_boolean_slow`
        // path), and a `+` whose `valueOf`/`toString` both return objects
        // (the TypeError rides the pending-error ABI and is caught in the
        // loop).
        let source = "function neg(n) { var s = 0; for (var i = 0; i < n; i++) { s += -i; } return s; }\n\
                      function pos(n) { var s = 0; for (var i = 0; i < n; i++) { s += +('' + i); } return s; }\n\
                      function bit(n) { var s = 0; for (var i = 0; i < n; i++) { s += ~i; } return s; }\n\
                      function nots(n) { var s = 0; for (var i = 0; i < n; i++) { s += !i ? 1 : 0; s += !'' ? 10 : 0; s += !({}) ? 100 : 0; } return s; }\n\
                      function big(n) { var b = 5n; var s = 0; for (var i = 0; i < n; i++) { s += Number(-b) + Number(~b); } return s; }\n\
                      function err(n) { var o = { valueOf: function () { return {}; }, toString: function () { return {}; } }; var c = 0; for (var i = 0; i < n; i++) { try { +o; } catch (e) { c++; } } return c; }\n\
                      neg(5) + pos(4) + bit(3) + nots(2) + big(1) + err(3);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the unary lowering must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_compiles_a_loop_whose_only_odd_step_is_unary() {
        // A script with no functions: its loop is the ONLY body the cache can
        // compile, so a missing `Unary` arm leaves the count at 0 (the bail
        // is sticky — the body never re-enters the JIT).
        let source = "var s = 0; for (var i = 0; i < 8; i++) { s += -i; } s;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(value, interp, "the compiled unary loop must match");
        assert_eq!(compiled, 1, "the script body itself must compile");
    }

    #[test]
    fn installed_jit_statement_local_updates_match_the_interpreter() {
        // Statement-position local updates (`l++;`, `++l;`, `l--;`) now fuse
        // into the loop body's register run (`LeafOp::UpdateReg`), so the
        // compiled body carries the op: the interpreter's inline f64 update
        // and the machine code's mirror must agree over postfix/prefix/
        // decrement and a numeric-string counter (the slow ToNumeric path).
        let source = "function f(n) { var l = '5'; var s = 0; for (var i = 0; i < n; i++) { s += i; l++; } return s; }\n\
                     function g(n) { var l = 0; for (var i = 0; i < n; i++) { ++l; l--; l++; } return l; }\n\
                     f(7) + g(7);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the compiled body must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_integer_operators_match_the_interpreter() {
        // The six integer operators inline for two Numbers inside the
        // truncating conversion's range, guarded so anything outside it (and
        // any non-Number) falls back to `binary_slow`. Covered: the inline path
        // over a spread of integers and a fraction, the guard's fallbacks
        // (`2^63`, `1e300`, `NaN`, the infinities), the coercing operands
        // (`'8'`, `'x'`, `true`, `null`, `undefined`), `>>>`'s `ToUint32` over
        // negatives, BigInt operands (the guard sends them to the helper) and
        // `>>>`'s BigInt TypeError.
        let source = "var V = [0, -0, 1, -1, 7, 255, 1023, 2147483647, -2147483648, 4294967296, -4294967296, 9007199254740991, 1e21, 9223372036854775808, 1e300, NaN, Infinity, -Infinity, 3.5, -3.5, 0.5, -0.5, '8', 'x', true, null, undefined];\n\
                      function ops(n) { var a = 0, b = 0, c = 0, d = 0, e = 0, f = 0; for (var k = 0; k < n; k++) { for (var i = 0; i < V.length; i++) { var x = V[i]; var sh = V[(i + 3) % V.length]; a = a + (x & 255); b = b + (x | 7); c = c ^ x; d = d + (x << sh); e = e + (x >> sh); f = f + (x >>> sh); } } return a + ',' + b + ',' + c + ',' + d + ',' + e + ',' + f; }\n\
                      function bigs(n) { var s = 0; for (var k = 0; k < n; k++) { s = s + Number(5n & 3n) + Number(5n | 3n) + Number(5n ^ 3n) + Number(5n << 2n) + Number(5n >> 1n); } return s; }\n\
                      function bigushr(n) { var c = 0; for (var k = 0; k < n; k++) { try { 5n >>> 1n; } catch (e) { c++; } } return c; }\n\
                      ops(3) + '|' + bigs(2) + '|' + bigushr(3);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the integer-operator lowering must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_int32_accumulator_matches_the_interpreter() {
        // The int32 register lane: `s = (s + rhs) | 0` (or `& mask`) runs as
        // wrapping i32 in the shared num-slot register. Covered: an int32
        // immediate, the bounded counter (A1), a mask, the wrap across the
        // int32 boundary (`wrap`, `mul`), a negative seed, the A2 prefix proof
        // (`band`/`bor`/`shl`/`shr`/`ushr`/`xor` — a bitwise rhs on a proven
        // counter folds to `CounterBit`; `slotrhs`/`bandSlot` keep the prefix
        // and prove `Int`), the guarded counter (`ctrSlot`/`negSlot`, a slot
        // limit), the `Mul` bound (`mulBig`'s large constant rhs is excluded
        // because the f64 product rounds past 2^53), a counter past the guard
        // (`huge` takes the exact slow path), and the proof's rejections (a
        // non-integral rhs and a fractional seed stay on the step path).
        let source = "function imm() { var s = 0; for (var i = 0; i < 1000; i++) { s = (s + 7) | 0; } return s; }\n\
                      function ctr() { var s = 0; for (var i = 0; i < 1000; i++) { s = (s + i) | 0; } return s; }\n\
                      function mask() { var s = 0; for (var i = 0; i < 1000; i++) { s = (s + i) & 1073741823; } return s; }\n\
                      function wrap() { var s = 0; for (var i = 0; i < 100; i++) { s = (s + 2000000000) | 0; } return s; }\n\
                      function neg() { var s = -1000; for (var i = 0; i < 1000; i++) { s = (s - i) | 0; } return s; }\n\
                      function mul() { var s = 3; for (var i = 0; i < 20; i++) { s = (s * 3) | 0; } return s; }\n\
                      function band() { var s = 0; for (var i = 0; i < 1000; i++) { s = (s + (i & 255)) | 0; } return s; }\n\
                      function bor() { var s = 0; for (var i = 0; i < 1000; i++) { s = (s + (i | 0)) | 0; } return s; }\n\
                      function shl() { var s = 0; for (var i = 0; i < 1000; i++) { s = (s + (i << 3)) | 0; } return s; }\n\
                      function shr() { var s = 0; for (var i = 0; i < 1000; i++) { s = (s + (i >> 3)) | 0; } return s; }\n\
                      function ushr() { var s = 0; for (var i = 0; i < 1000; i++) { s = (s + (i >>> 1)) | 0; } return s; }\n\
                      function xr() { var s = 0; for (var i = 0; i < 1000; i++) { s = (s + (i ^ 12345)) | 0; } return s; }\n\
                      function bandSlot(n) { var s = 0; for (var i = 0; i < n; i++) { s = (s + (i & 255)) | 0; } return s; }\n\
                      function slotrhs(n) { var s = 0; for (var i = 0; i < n; i++) { var k = i & 255; s = (s + k) | 0; } return s; }\n\
                      function ctrSlot(n) { var s = 0; for (var i = 0; i < n; i++) { s = (s + i) | 0; } return s; }\n\
                      function negSlot(n) { var s = -1000; for (var i = 0; i < n; i++) { s = (s - i) | 0; } return s; }\n\
                      function mulBig() { var s = 1073741825; for (var i = 0; i < 1; i++) { s = (s * 1073741825) | 0; } return s; }\n\
                      function huge(n) { var s = 0; for (var i = 4503599627370496; i < n; i++) { s = (s + i) | 0; } return s; }\n\
                      function frac(n) { var s = 1.5; for (var i = 0; i < n; i++) { s = (s + i) | 0; } return s; }\n\
                      function nonint() { var s = 0; for (var i = 0; i < 1000; i++) { s = (s + (i * 1.5)) | 0; } return s; }\n\
                      [imm(), ctr(), mask(), wrap(), neg(), mul(), band(), bor(), shl(), shr(), ushr(), xr(), bandSlot(1000), slotrhs(1000), ctrSlot(1000), negSlot(1000), mulBig(), huge(4503599627370499), frac(1000), nonint()].join(',');";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the int32 accumulator lane must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_integral_rem_matches_the_interpreter() {
        // `plan_loop_rem` folds a proven-integral `% <nonzero const>` into
        // `LeafOp::BinRemConst`, which the JIT runs as an `srem` behind a lone
        // `|acc| < 2^63` range guard. Covered: a positive dividend, a negative
        // one (fmod's dividend sign), a non-constant divisor (no rewrite), a
        // `% 0` (NaN, no rewrite), and the guard's boundary (a value past the
        // i64 domain falls to the exact `binary_slow`).
        let source = "function mulsign() { var s = 0; for (var i = 0; i < 1000; i++) { s = s + ((i * 31 + 5) % 7); } return s; }\n\
                      function negsign() { var s = 0; for (var i = 0; i < 1000; i++) { s = s + ((5 - i * 31) % 7); } return s; }\n\
                      function varMod() { var d = 7; var s = 0; for (var i = 0; i < 1000; i++) { s = s + ((i * 31 + 5) % d); } return s; }\n\
                      function zeroMod() { var s = 0; for (var i = 0; i < 5; i++) { s = s + ((i * 31 + 5) % 0); } return String(s); }\n\
                      function bigMod() { var s = 0; for (var i = 0; i < 3; i++) { s = s + (1e20 % 7); } return s; }\n\
                      [mulsign(), negsign(), varMod(), zeroMod(), bigMod()].join(',');";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the integral-remainder lowering must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_math_intrinsics_match_the_interpreter() {
        // The Stage-B `Math.<name>` splice: a `.<name>(...)` call whose
        // resolved callee is the realm's `%Math.<name>%` computes the operation
        // in machine code. The identity check is the retirement — a shadowed
        // `<name>`, a non-`Math` receiver and a non-Number argument all take
        // the general call — so each of those must agree with the interpreter.
        let source = "var out = [];\n\
                      function t(v) { out.push(String(v)); }\n\
                      t(Math.abs(5)); t(Math.abs(-5)); t(Math.abs(-0)); t(Math.abs(NaN));\n\
                      t(Math.abs(Infinity)); t(Math.abs('5')); t(Math.abs('abc')); t(Math.abs(5, 9));\n\
                      var o = { valueOf: function () { return -7; } }; t(Math.abs(o));\n\
                      t(Math.ceil(1.2)); t(Math.ceil(-1.2)); t(1 / Math.ceil(-0.5));\n\
                      t(Math.floor(1.8)); t(Math.floor(-1.2)); t(1 / Math.floor(0.5));\n\
                      t(Math.trunc(1.7)); t(Math.trunc(-1.7)); t(1 / Math.trunc(-0.5));\n\
                      t(Math.sqrt(2)); t(Math.sqrt(-1)); t(1 / Math.sqrt(-0));\n\
                      t(Math.floor('3.9'));\n\
                      var saved = Math.floor; Math.floor = function (x) { return x + 100; }; t(Math.floor(-5));\n\
                      Math.floor = saved; t(Math.floor(-5.5));\n\
                      t({ floor: Math.floor }.floor(-3.2));\n\
                      t({ ceil: function (x) { return x * 2; } }.ceil(2.5));\n\
                      function loop(n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) { s = (s + Math.floor(i * 0.5)) | 0; }\n\
                        return s;\n\
                      }\n\
                      t(loop(1000));\n\
                      out.join(',');";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the Math intrinsic splices must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_char_code_at_intrinsic_matches_the_interpreter() {
        // The Stage-B `String.prototype.charCodeAt` splice: a `.charCodeAt(...)`
        // call whose resolved callee is the realm's intrinsic and whose receiver
        // is a String primitive computes the code unit in machine code. The
        // identity check is the retirement and the receiver guard the soundness
        // gate — a shadowed method, a non-String receiver, and a non-Number
        // position all take the general call — so each must agree with the
        // interpreter.
        let source = "var out = [];\n\
                      function t(v) { out.push(String(v)); }\n\
                      t('hello'.charCodeAt(0)); t('hello'.charCodeAt(4)); t('hello'.charCodeAt(5));\n\
                      t('hello'.charCodeAt(-1)); t('hello'.charCodeAt(1.9)); t('hello'.charCodeAt(NaN));\n\
                      t('hello'.charCodeAt(-0.5)); t('hello'.charCodeAt(-0));\n\
                      t(''.charCodeAt(0)); t('a😀b'.charCodeAt(1));\n\
                      t('hello'.charCodeAt('1')); t('hello'.charCodeAt(1, 9)); t('hello'.charCodeAt());\n\
                      var saved = String.prototype.charCodeAt;\n\
                      String.prototype.charCodeAt = function (i) { return 999; };\n\
                      t('hello'.charCodeAt(1));\n\
                      String.prototype.charCodeAt = saved; t('hello'.charCodeAt(1));\n\
                      t({ charCodeAt: String.prototype.charCodeAt }.charCodeAt(0));\n\
                      t({ charCodeAt: function (i) { return i; } }.charCodeAt(7));\n\
                      function loop(n) {\n\
                        var strs = ['abcdefgh', 'ijklmnop', 'qrstuvwx', 'yz012345'];\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) { var k = i & 1023; s = (s + strs[k & 3].charCodeAt(k & 7)) | 0; }\n\
                        return s;\n\
                      }\n\
                      t(loop(1000));\n\
                      out.join(',');";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the charCodeAt intrinsic splice must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_array_index_of_intrinsic_matches_the_interpreter() {
        // The Stage-B `Array.prototype.indexOf` splice: a `.indexOf(...)` call
        // whose resolved callee is the realm's intrinsic scans a dense array in
        // a helper. The helper declines (the general call runs) for a hole, a
        // sparse/non-Array receiver, or a non-Number `from`, so each must agree
        // with the interpreter.
        let source = "var out = [];\n\
                      function t(v) { out.push(String(v)); }\n\
                      t([1,2,3,4].indexOf(3)); t([1,2,3,4].indexOf(9));\n\
                      t([1,2,3,4].indexOf(2, 1)); t([1,2,3,4].indexOf(1, 1));\n\
                      t([1,2,3,4].indexOf(4, -1)); t([1,2,3,4].indexOf(1, -100)); t([1,2,3,4].indexOf(1, 100));\n\
                      var a = [1,,3]; t(a.indexOf(3)); t(a.indexOf(undefined)); t(a.indexOf(undefined, 1));\n\
                      t([NaN].indexOf(NaN)); t([0].indexOf(-0)); t([1,'1'].indexOf('1'));\n\
                      t([1,2,3].indexOf('2')); t([1,2,3].indexOf(2, '1'));\n\
                      t(Array.prototype.indexOf.call({length:1, 0:3}, 3));\n\
                      var saved = Array.prototype.indexOf;\n\
                      Array.prototype.indexOf = function () { return 999; };\n\
                      t([1,2,3].indexOf(2));\n\
                      Array.prototype.indexOf = saved; t([1,2,3].indexOf(2));\n\
                      t({ indexOf: Array.prototype.indexOf }.indexOf(0));\n\
                      function loop(n) {\n\
                        var small = [1, 2, 3, 4];\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) { s = (s + small.indexOf(3)) | 0; }\n\
                        return s;\n\
                      }\n\
                      t(loop(1000));\n\
                      out.join(',');";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the indexOf intrinsic splice must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_map_set_intrinsics_match_the_interpreter() {
        // The Stage-B `Map.prototype.get` / `Set.prototype.has` splices: a
        // `.get(...)`/`.has(...)` call whose resolved callee is the realm's
        // intrinsic probes the collection in a helper. A non-collection receiver
        // makes the helper decline (the general call throws the exact TypeError),
        // so a `call`/own-method receiver must agree with the interpreter too.
        let source = "var out = [];\n\
                      function t(v) { out.push(String(v)); }\n\
                      function tc(f) { try { f(); out.push('no-throw'); } catch (e) { out.push(e instanceof TypeError ? 'TypeError' : 'other'); } }\n\
                      var m = new Map(); m.set(1, 'a'); m.set('k', 2); m.set(NaN, 3); m.set(0, 'z');\n\
                      t(m.get(1)); t(m.get('k')); t(m.get(NaN)); t(m.get(9)); t(m.get(-0));\n\
                      var set = new Set(); set.add(1); set.add('k'); set.add(NaN); set.add(0);\n\
                      t(set.has(1)); t(set.has('k')); t(set.has(NaN)); t(set.has(9)); t(set.has(-0));\n\
                      tc(function () { Map.prototype.get.call({}, 1); });\n\
                      tc(function () { Set.prototype.has.call({}, 1); });\n\
                      tc(function () { ({get: Map.prototype.get}).get(1); });\n\
                      var savedG = Map.prototype.get; Map.prototype.get = function () { return 42; };\n\
                      t(m.get(1));\n\
                      Map.prototype.get = savedG; t(m.get(1));\n\
                      function loopMap(n) {\n\
                        var mm = new Map(); for (var i = 0; i < 1024; i++) mm.set(i, i);\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) { var k = i & 1023; s = (s + mm.get(k)) | 0; }\n\
                        return s;\n\
                      }\n\
                      t(loopMap(1000));\n\
                      function loopSet(n) {\n\
                        var ss = new Set(); for (var i = 0; i < 1024; i++) ss.add(i);\n\
                        var c = 0;\n\
                        for (var i = 0; i < n; i++) { var k = i & 1023; c = (c + (ss.has(k) ? 1 : 0)) | 0; }\n\
                        return c;\n\
                      }\n\
                      t(loopSet(1000));\n\
                      var m2 = new Map(); t(m2.set(1, 'a') === m2); t(m2.get(1));\n\
                      m2.set(1, 'b'); t(m2.get(1)); m2.set(NaN, 1); t(m2.get(NaN));\n\
                      m2.set(-0, 'z'); t(m2.get(0)); m2.set(2); t(m2.get(2));\n\
                      tc(function () { Map.prototype.set.call({}, 1, 2); });\n\
                      var savedS = Map.prototype.set; Map.prototype.set = function () { return 'x'; };\n\
                      t(m2.set(2, 'q')); t(m2.get(2));\n\
                      Map.prototype.set = savedS; t(m2.set(2, 'w') === m2); t(m2.get(2));\n\
                      function loopSet2(n) {\n\
                        var mm = new Map();\n\
                        for (var i = 0; i < n; i++) { mm.set(i & 1023, i); }\n\
                        return mm.get(5);\n\
                      }\n\
                      t(loopSet2(5000));\n\
                      out.join(',');";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the Map.get/Set.has intrinsic splices must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_array_at_includes_intrinsics_match_the_interpreter() {
        // The Stage-B `Array.prototype.at` / `Array.prototype.includes` splices:
        // dense siblings of `indexOf`. `at` returns the element (or `undefined`),
        // `includes` a Boolean; both decline (the general call runs) for a hole,
        // a sparse/non-Array receiver, or a non-Number index/`from`.
        let source = "var out = [];\n\
                      function t(v) { out.push(String(v)); }\n\
                      function tc(f) { try { f(); out.push('no-throw'); } catch (e) { out.push(e instanceof TypeError ? 'TypeError' : 'other'); } }\n\
                      t([1,2,3,4].at(2)); t([1,2,3,4].at(-1)); t([1,2,3,4].at(-5));\n\
                      t([1,2,3,4].at(9)); t([1,2,3,4].at(1.9)); t([1,2,3,4].at(-0));\n\
                      var h = [1,,3]; t(h.at(1)); t(h.includes(undefined));\n\
                      t([1,2,3,4].includes(4)); t([1,2,3,4].includes(9));\n\
                      t([NaN].includes(NaN)); t([0].includes(-0)); t(['1'].includes(1));\n\
                      t([1,2,3,4].includes(3, 3)); t([1,2,3,4].includes(3, 4)); t([1,2,3,4].includes(1, -1));\n\
                      t(Array.prototype.at.call({length:2, 1:'x'}, 1));\n\
                      t(Array.prototype.includes.call({length:2, 1:'x'}, 'x'));\n\
                      tc(function () { Array.prototype.at.call(null, 1); });\n\
                      var savedA = Array.prototype.at; Array.prototype.at = function () { return 'A'; };\n\
                      t([1,2,3].at(1));\n\
                      Array.prototype.at = savedA; t([1,2,3].at(1));\n\
                      var savedI = Array.prototype.includes; Array.prototype.includes = function () { return 'I'; };\n\
                      t([1,2,3].includes(2));\n\
                      Array.prototype.includes = savedI; t([1,2,3].includes(2));\n\
                      function loop(n) { var a = [1,2,3,4]; var s = 0;\n\
                        for (var i = 0; i < n; i++) { s = (s + a.at(2)) | 0; s = (s + (a.includes(4) ? 1 : 0)) | 0; }\n\
                        return s; }\n\
                      t(loop(1000));\n\
                      out.join(',');";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the Array.at/includes intrinsic splices must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_array_push_intrinsic_matches_the_interpreter() {
        // The Stage-B `Array.prototype.push` splice (single element): a
        // `.push(x)` call whose resolved callee is the realm's intrinsic appends
        // through the dense element write and returns the new length. The helper
        // declines for a non-Array/non-dense receiver or a length overflow, and
        // the multi-element/zero-element forms keep the general call.
        let source = "var out = [];\n\
                      function t(v) { out.push(String(v)); }\n\
                      function tc(f) { try { f(); out.push('no-throw'); } catch (e) { out.push(e instanceof TypeError ? 'TypeError' : 'other'); } }\n\
                      var a = []; t(a.push(1)); t(a.push(2)); t(a.length); t(a.join(','));\n\
                      t([].push()); t([1,2].push(3,4));\n\
                      var h = [1,,3]; t(h.push(4)); t(h.length);\n\
                      t(Array.prototype.push.call({length:2, 0:'a', 1:'b'}, 'c'));\n\
                      t(Array.prototype.push.call({length:0}, 1));\n\
                      var big = {length: 9007199254740991}; tc(function () { Array.prototype.push.call(big, 1); });\n\
                      var savedP = Array.prototype.push; Array.prototype.push = function () { return 'P'; };\n\
                      t(a.push(9));\n\
                      Array.prototype.push = savedP; t(a.push(9));\n\
                      function loop(n) { var a2 = []; var s = 0;\n\
                        for (var i = 0; i < n; i++) { a2.push(i); if (a2.length > 1000) a2.length = 0; s = (s + 1) | 0; }\n\
                        return s; }\n\
                      t(loop(5000));\n\
                      out.join(',');";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the Array.push intrinsic splice must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_shared_ctx_construct_leaf_recovers_after_a_throw() {
        // The shared-ctx construct leaf: a compiled loop's `new C(i)` runs an
        // environment-free leaf body on the CALLER's ctx with a frame carved
        // from its buffer (no per-construct ctx rebuild). The body CALLS a
        // helper (an internal leaf-call site on the shared ctx) and throws
        // at one iteration; the pending byte must route to the caller's
        // catch and the loop must construct cleanly AFTER the throw (the
        // shared ctx's error/pending state and the caller's leaf-cache slots
        // are left consistent).
        let source = "function h(x) { return x * 2; }\n\
                     function C(x) { this.a = h(x); if (x === 50) { throw new Error('boom'); } this.b = x; }\n\
                     var s = 0, c = 0;\n\
                     for (var i = 0; i < 100; i++) { try { var o = new C(i); s += o.a + o.b; } catch (e) { c++; } }\n\
                     s * 1000 + c * 10 + new C(7).a;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the shared-ctx construct leaf must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_fused_slot_compounds_match_the_interpreter() {
        // A statement-position slot compound whose RHS resolved into the
        // accumulator (`s += i`) now fuses the `[BinLeftReg, StoreReg]` tail
        // into one `BinStoreReg` op, so the compiled body carries it: the
        // machine-code mirror (combine + store back) must agree with the
        // interpreter over the inline number path and the string-concat slow
        // path inside the single op.
        let source = "function f(n) { var s = 0; for (var i = 0; i < n; i++) { s += i; } return s; }\n\
                     function g(n) { var s = ''; for (var i = 0; i < n; i++) { s += i; } return s.length; }\n\
                     function h(n) { var s = 1; for (var i = 0; i < n; i++) { s = s + i; } return s; }\n\
                     f(100) + g(100) + h(3);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the compiled body must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_strict_eq_conditions_match_the_interpreter() {
        // The fused `JumpIfEqImm`/`JumpIfNeqImm` step (an `if (x === N)` /
        // `x !== N` test in one dispatch) must agree with the interpreter
        // over Numbers, a numeric-string (never `===` a Number), and `!==`,
        // in loops and as a `while` condition.
        let source = "function f(n) { var l = '5'; var c = 0; for (var i = 0; i < n; i++) { if (l === 5) { c++; } } return c; }\n\
                     function g(n) { var l = 0; var c = 0; for (var i = 0; i < n; i++) { l++; if (l === 10000) { c++; } } return c; }\n\
                     function h(n) { var l = 0; var c = 0; while (l !== n) { c++; l++; } return c; }\n\
                     f(7) + g(20000) * 1000 + h(6);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the compiled body must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_typed_array_length_probe_serves_compiled_reads() {
        // A loop reading `ta.length` per iteration compiles, and the
        // compiled `GetMemberName`-with-length probe serves the slots length
        // with no `get_member_name` round-trip. A plain-object receiver
        // misses the probe (the sentinel) and falls back to the member-cell
        // probe / helper — behavior unchanged.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(x) { var s = 0; for (var i = 0; i < 1000; i++) { s += x.length; } return s; } \
                     f({ length: 5 }); f(new Uint8Array(7));",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(7000.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_array_length_read_skips_the_ffi_probe() {
        // A compiled loop reading `a.length` over a dense Array must be served
        // by the machine-code dense probe, not the `typed_array_length` FFI
        // probe: the counting wrapper proves the FFI probe never runs for the
        // inlined shape. Routing the typed-array probe's miss straight to the
        // FFI helper (the pre-change order) makes this count one call per
        // iteration, so the assertion is the regression guard.
        static LENGTH_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_length(ctx: *mut c_void, object: u64) -> u64 {
            LENGTH_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.typed_array_length)(ctx, object)
        }
        let mut helpers = runtime_helpers();
        helpers.typed_array_length = Some(counting_length);

        let source = "function sum(a, n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) { s += a.length; }\n\
                        return s;\n\
                      }\n\
                      var a = [1, 2, 3, 4, 5, 6, 7, 8];\n\
                      sum(a, 100000) + a.length;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value, interp, "the length read must match the interpreter");
        assert!(compiled >= 1, "{compiled} bodies must compile");
        let calls = LENGTH_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            calls < 100,
            "the compiled loop must serve the dense `length` from the slots ({calls} typed_array_length calls)"
        );
    }

    #[test]
    fn installed_jit_dense_for_of_inlines_the_cursor() {
        // A compiled `for (const v of a)` over a dense Array must advance the
        // machine-code fast cursor: the counting wrapper proves the
        // per-element `for_of_next_bind_local` helper never runs (only the
        // per-loop decline does, through `for_of_fast_next`). Disabling the
        // inline lowering makes the count one call per element.
        static BIND_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_bind(ctx: *mut c_void, slot: u64) -> u64 {
            BIND_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.for_of_next_bind_local)(ctx, slot)
        }
        let mut helpers = runtime_helpers();
        helpers.for_of_next_bind_local = Some(counting_bind);

        let source = "function sum(a, reps) {\n\
                        var s = 0;\n\
                        for (var r = 0; r < reps; r++) {\n\
                          for (const v of a) { s += v; }\n\
                        }\n\
                        return s;\n\
                      }\n\
                      sum([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 10000);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value, interp, "the dense for-of must match the interpreter");
        assert!(compiled >= 1, "{compiled} bodies must compile");
        let calls = BIND_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            calls < 100,
            "the compiled for-of must inline the dense cursor ({calls} for_of_next_bind_local calls)"
        );
    }

    #[test]
    fn installed_jit_dense_for_of_matches_the_interpreter() {
        // The inline fast cursor against every decline shape: a dense Array
        // (the served shape), a middle hole, a trailing hole (`length` past
        // `elem_len`), a sparse (non-dense) Array, non-Array iterables (a Set,
        // a String, a TypedArray), a mid-loop grow and shrink, a `break`, and
        // nesting. Each variant's contribution is folded into one checksum, so
        // any divergence from the interpreter moves the value.
        let source = "function run() {\n\
  var out = 0;\n\
  for (const v of [1, 2, 3, 4, 5]) out = out * 3 + v;\n\
  for (const v of [1, , 3]) out = out * 3 + (v === undefined ? 7 : v);\n\
  var t = [1, 2]; t.length = 5;\n\
  for (const v of t) out = out * 3 + (v === undefined ? 9 : v);\n\
  var s = []; s[3] = 4;\n\
  for (const v of s) out = out * 3 + (v === undefined ? 5 : v);\n\
  for (const v of new Set([1, 2, 3])) out = out * 3 + v;\n\
  for (const v of 'abc') out = out * 3 + v.charCodeAt(0);\n\
  for (const v of new Int32Array([4, 5])) out = out * 3 + v;\n\
  var m = [1, 2];\n\
  for (const v of m) { out = out * 3 + v; if (m.length < 4) m.push(9); }\n\
  var q = [1, 2, 3, 4];\n\
  for (const v of q) { out = out * 3 + v; if (q.length > 2) q.length = 2; }\n\
  for (const v of [1, 2, 3, 4, 5]) { if (v === 3) break; out = out * 3 + v; }\n\
  for (const x of [[1, 2], [3]]) for (const y of x) out = out * 3 + y;\n\
  var fns = [];\n\
  for (const v of [1, 2, 3]) fns.push(function () { return v; });\n\
  out = out * 3 + fns[0]() * 100 + fns[1]() * 10 + fns[2]();\n\
  return out;\n\
}\n\
run(); run(); run(); run(); run(); run(); run(); run(); run();\n\
run(); run(); run(); run(); run(); run(); run(); run(); run();";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "every for-of shape must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies must compile");
    }

    #[test]
    fn installed_jit_runs_a_vector_self_tail_call() {
        // Cut 51/M2: a self-tail-call with 65 plain arguments (beyond the
        // fast form's `FAST_CALL_MAX_ARGS` cap of 64) compiles to the
        // vector self-jump — the whole recursive chain runs in ONE
        // machine-code invocation with a bounded native stack.
        let (value, compiled) = with_jit_agent(|agent| {
            agent.run_script(
                "\"use strict\"; (function f(n, a, b, c, d, e, g, h, i, j, k, l, m, o, p, q, r, s, t, u, v, w, x, y, z, A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z, a0, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14) { return n ? f(n - 1, a, b, c, d, e, g, h, i, j, k, l, m, o, p, q, r, s, t, u, v, w, x, y, z, A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z, a0, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14) : a + b + c + d + e + g + h + i + j + k + l + m + o + p + q + r + s + t + u + v + w + x + y + z + A + B + C + D + E + F + G + H + I + J + K + L + M + N + O + P + Q + R + S + T + U + V + W + X + Y + Z + a0 + a1 + a2 + a3 + a4 + a5 + a6 + a7 + a8 + a9 + a10 + a11 + a12 + a13 + a14; }(50000, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65));",
            )
            .expect("runs")
        });
        assert_eq!(value.as_number(), Some(2145.0));
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
        // machine code, with holes and a spread. Cut 69: both bodies are
        // straight-line, so the expression repeats 17× to cross the
        // promotion threshold.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { return [1, , 3]; } \
                     var g = function () { return [1, ...[2, 3]].length; }; \
                     f().length + g(); f().length + g(); f().length + g(); f().length + g(); \
                     f().length + g(); f().length + g(); f().length + g(); f().length + g(); \
                     f().length + g(); f().length + g(); f().length + g(); f().length + g(); \
                     f().length + g(); f().length + g(); f().length + g(); f().length + g(); \
                     f().length + g();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(6.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_an_object_literal_body() {
        // Cut 53: an object-literal body compiles — plain, computed-key, and
        // spread properties built in machine code. Cut 69: `f` is
        // straight-line, so the call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { var k = 2; return { a: 1, [k]: 3, ...{ d: 4 } }; } \
                     var o = f(); var o = f(); var o = f(); var o = f(); var o = f(); var o = f(); \
                     var o = f(); var o = f(); var o = f(); var o = f(); var o = f(); var o = f(); \
                     var o = f(); var o = f(); var o = f(); var o = f(); var o = f(); \
                     o.a + o[2] + o.d;",
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
        // Cut 69: `f` itself is not certified (its object-literal body stays
        // env-path); the METHOD bodies are straight-line, so the calls
        // repeat 17× against the ONE hoisted object.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { return { m() { return 4; }, get g() { return 5; } }; } \
                     var o = f(); \
                     o.m() + o.g; o.m() + o.g; o.m() + o.g; o.m() + o.g; o.m() + o.g; o.m() + o.g; \
                     o.m() + o.g; o.m() + o.g; o.m() + o.g; o.m() + o.g; o.m() + o.g; o.m() + o.g; \
                     o.m() + o.g; o.m() + o.g; o.m() + o.g; o.m() + o.g; o.m() + o.g;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(9.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_fused_object_literal_matches_the_interpreter() {
        // Cut 72: a whole-simple literal lowers to ONE fused `ObjectFast`
        // helper (not `ObjectBegin` + N per-key defines) — a compiled loop
        // creating a 5-key and a 20-key literal every iteration (the first
        // INLINE_FIELDS keys adopt vector-free, the tail defines) must agree
        // with the interpreter on the field values and the grow-after
        // shape. The key-order probes make the created objects observable.
        let source = "function f(n) { var s = 0; \
                      for (var i = 0; i < n; i++) { \
                        var o = { a: i, b: i + 1, c: i + 2, d: i + 3, e: i + 4 }; \
                        s += o.a + o.e; \
                        var p = { a: i, b: i, c: i, d: i, e: i, f: i, g: i, h: i, i9: i, j: i, \
                                  k: i, l: i, m: i, n9: i, o9: i, p9: i, q: i, r: i, s9: i, t: i }; \
                        if (Object.keys(p).join(',') !== 'a,b,c,d,e,f,g,h,i9,j,k,l,m,n9,o9,p9,q,r,s9,t') { s += 1000; } \
                        if (p.t !== i || p.q !== i || p.a !== i) { s += 2000; } \
                        p.u = i; \
                        if (p.u !== i || Object.keys(p).length !== 21) { s += 3000; } \
                      } return s; } \
                      f(2000);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the compiled fused-object-literal creates must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_fused_array_literal_matches_the_interpreter() {
        // The array analogue of the fused-object-literal e2e: a whole-simple
        // literal lowers to ONE fused `ArrayFast` helper (not `ArrayBegin` +
        // N per-element `ArrayElement` + `ArrayEnd`) — a compiled loop
        // creating empty, nested, and longer literals every iteration must
        // agree with the interpreter on the values and `length`. The unfused
        // shapes (a hole, a spread) stay on the per-element path and must
        // also agree.
        let source = "function f(n) { var s = 0; \
                      for (var i = 0; i < n; i++) { \
                        var a = [i, i + 1, i + 2]; \
                        s += a[0] + a[2] + a.length; \
                        var e = []; \
                        s += e.length; \
                        var nested = [[i, i + 1], i + 2]; \
                        s += nested[0][1] + nested[1] + nested.length; \
                        var h = [i, , i + 2]; \
                        s += h[0] + h[2] + h.length + (1 in h ? 1000 : 0); \
                        var sp = [...a, i]; \
                        s += sp[3] + sp.length; \
                      } return s; } \
                      f(2000);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the compiled fused-array-literal creates must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_an_empty_loop_body() {
        // Cut 72 follow-up: an empty certified for-body must terminate in
        // compiled code. The head's backward edge has to re-enter a DISTINCT
        // body block; if `body_start` collapses onto the head's own step the
        // compiled induction variable never advances and the body spins
        // forever (the empty-block and empty-statement forms both).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(n) { for (var i = 0; i < n; i++) {} return 1; } \
                     function g(n) { for (var i = 0; i < n; i++); return 2; } \
                     f(100000) + g(100000);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(3.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_testless_for_head() {
        // A `for (;;)` head has no test value to push: the old dummy test
        // push leaked one working-stack slot per iteration, and the compiled
        // loop ran past its fixed buffer (a segfault after ~64 iterations).
        // This must complete 100k iterations in compiled code.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { var c = 0; for (;;) { c++; if (c === 100000) break; } return c; } \
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(100000.0));
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
        // `ConcatStrConst` — compiles. Cut 69: `f` is straight-line, so the
        // call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(n) { return `a${n}b`.length; } \
                     f(7); f(7); f(7); f(7); f(7); f(7); f(7); f(7); f(7); f(7); \
                     f(7); f(7); f(7); f(7); f(7); f(7); f(7);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(3.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_template_rope_matches_the_interpreter() {
        // The template append is a ROPE concat now (`JsString::concat`; a
        // 12-substitution result ropes well past the 128-unit threshold), so
        // the content must still equal the interpreter's exactly — including
        // an astral pair and a LONE surrogate, which a lossy UTF-8 round-trip
        // would replace with U+FFFD. `s === s.slice(0)` and `s === f(...)`
        // compare a rope against a flat rebuild inside the engine, so a
        // representation-only divergence would drop those terms.
        let source = "function f(a, i) { \
                        return `${a}${i}-${a}${i}-${a}${i}-${a}${i}-${a}${i}-${a}${i}\
                        -${a}${i}-${a}${i}-${a}${i}-${a}${i}-${a}${i}-${a}${i}`; } \
                      f('x', 1); f('x', 1); f('x', 1); f('x', 1); f('x', 1); f('x', 1); f('x', 1); f('x', 1); \
                      f('x', 1); f('x', 1); f('x', 1); f('x', 1); f('x', 1); f('x', 1); f('x', 1); f('x', 1); \
                      var s = f('\\uD83D\\uDE00\\uD83D', 12345); \
                      var t = s.length * 1000 + s.charCodeAt(0) + s.charCodeAt(1) + s.charCodeAt(s.length - 1); \
                      t + (s === s.slice(0) ? 7 : 0) + (s === f('\\uD83D\\uDE00\\uD83D', 12345) ? 11 : 0);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "a rope template build must match the interpreter's content"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_declared_vector_self_tail_call() {
        // Cut 51: the checked vector form — a top-level declaration's own
        // name, 33 plain arguments; the identity check takes the self jump
        // and the whole chain runs in one machine-code invocation.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "\"use strict\"; function f(n, a, b, c, d, e, g, h, i, j, k, l, m, o, p, q, r, s, t, u, v, w, x, y, z, A, B, C, D, E, F, G, H, I) { \
                     return n ? f(n - 1, a, b, c, d, e, g, h, i, j, k, l, m, o, p, q, r, s, t, u, v, w, x, y, z, A, B, C, D, E, F, G, H, I) : \
                     a + b + c + d + e + g + h + i + j + k + l + m + o + p + q + r + s + t + u + v + w + x + y + z + A + B + C + D + E + F + G + H + I; \
                     } f(50000, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(561.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_slot_callee() {
        // `return g(x) + 1` — a param callee (fused `CallFastSlot`), no loop.
        // Cut 69: both bodies are straight-line, so the call repeats 17×
        // against the ONE hoisted callee site (the completion stays the
        // last call's value).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(g, x) { return g(x) + 1; } \
                     var g = function (x) { return x + 1; }; \
                     f(g, 41); f(g, 41); f(g, 41); f(g, 41); f(g, 41); f(g, 41); \
                     f(g, 41); f(g, 41); f(g, 41); f(g, 41); f(g, 41); f(g, 41); \
                     f(g, 41); f(g, 41); f(g, 41); f(g, 41); f(g, 41);",
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
    fn installed_jit_construct_in_a_loop_compiles_and_runs() {
        // `Step::Construct` in a certified loop body: `new Point(i)` per
        // iteration (the vector-form construct — ArgsBase/ArgsPush build the
        // Vm argument vector, the helper runs the construct machinery).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function Point(x) { this.x = x; }\n\
                     function bench(n) { var s = 0; for (var i = 0; i < n; i++) { var p = new Point(i); s += p.x; } return s; }\n\
                     bench(100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(4950.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_constructing_a_builtin_in_a_loop_compiles_and_runs() {
        // A builtin constructor (`new Uint8Array`) in a compiled loop: the
        // construct helper's general path (no EcmaScript leaf) plus the
        // typed-array element store/read on the fresh view each iteration.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function bench(n) { var s = 0; for (var i = 0; i < n; i++) { var a = new Uint8Array(4); a[0] = i & 255; s += a[0] + a.length; } return s; } bench(100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(5350.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_construct_error_in_a_compiled_loop_is_caught() {
        // A throwing construct inside a compiled body's own try: the helper
        // sets the pending byte and the machine code dispatches to the catch
        // without drifting the working stack.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function Boom() { throw new Error('boom'); }\n\
                     function bench(n) { var s = 0; for (var i = 0; i < n; i++) { try { new Boom(); } catch (e) { s++; } } return s; }\n\
                     bench(100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(100.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_labeled_break_out_of_an_acc_loop_syncs_the_counter() {
        // Cut 17 acc-path + the Break leaf exclusion: a labeled
        // `break`/`continue` to a label outside the loop body jumps past the
        // acc path's single end-step `FastLoopStore`, so such bodies now
        // fall back to the slot path (whose head writes the binding every
        // iteration) — after `break outer`, i must be 5, not the pre-loop 0.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { var s = 0; outer: for (var i = 0; i < 10; i++) { s += i; if (i === 5) break outer; } return s * 100 + i; }\n\
                     f();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_number(),
            Some(1505.0),
            "f() completion was {value:?}"
        );
        assert!(compiled >= 1, "{compiled} bodies");

        // The labeled-`continue` variant: an inner var-head loop jumps to
        // an enclosing loop's continue, skipping the inner loop's own end
        // step — j must be 2 (its value when the continue fired).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function g() { var s = 0; outer: for (var i = 2; i >= 0; i--) { for (var j = 0; j < 10; j++) { if (j === 2) continue outer; s++; } } return s * 10 + j; }\n\
                     g();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(62.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_break_out_of_a_fast_loop_does_not_corrupt_the_caller() {
        // The Break leaf exclusion: a leaf-inlined compiled run of a body
        // with a break out of a fast loop left the caller's Vm inconsistent
        // (the counter's single end-step sync was skipped on the break
        // dispatch path) — the call after the leaf returned wrong / blank
        // output. Such bodies now run the general path (their own
        // frame/buffer), which is exact: the plain-break loop returns its
        // sum and a later call is unaffected.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function q() { var s = 0; for (var i = 0; i < 10; i++) { if (i === 5) break; s += i; } return s; }\n\
                     function after() { return 7; }\n\
                     q() * 100 + after();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1007.0));
        assert!(compiled >= 1, "{compiled} bodies");
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
    fn installed_jit_apply_intrinsic_takes_the_dense_fast_path() {
        // M10: a loop calling `add.apply(null, arr)` with a dense array and
        // a leaf receiver — the compiled `CallApply` site recognizes the
        // realm's intrinsic (the ctx's per-run snapshot), the
        // `apply_args_fill` helper copies the elements into the working
        // buffer, and the leaf runs in-frame. The counting wrappers prove
        // the shape: the fill runs once per iteration and the `call_apply`
        // slow path never does.
        static FILL_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static SLOW_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_apply_args_fill(ctx: *mut c_void, arg_array: u64, dest: u64) -> u64 {
            FILL_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.apply_args_fill)(ctx, arg_array, dest)
        }
        extern "C" fn counting_call_apply(
            ctx: *mut c_void,
            resolved: u64,
            callee: u64,
            argc: u64,
            args: *mut u64,
            kind: u64,
        ) -> u64 {
            SLOW_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.call_apply)(ctx, resolved, callee, argc, args, kind)
        }
        let mut helpers = runtime_helpers();
        helpers.apply_args_fill = Some(counting_apply_args_fill);
        helpers.call_apply = Some(counting_call_apply);
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
                "function add(a, b, c, d, e) { return a + b + c + d + e; }\n\
                 function run() {\n\
                   var s = 0; var arr = [1, 2, 3, 4, 5];\n\
                   for (var i = 0; i < 1000; i++) { s += add.apply(null, arr); }\n\
                   return s;\n\
                 }\n\
                 run();",
            )
            .expect("runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value.as_number(), Some(15000.0));
        assert!(compiled >= 2, "{compiled} bodies (run + add) must compile");
        let fills = FILL_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(fills, 1000, "the dense fill runs per iteration");
        let slows = SLOW_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(slows, 0, "the intrinsic fast path never hits `call_apply`");
    }

    #[test]
    fn installed_jit_call_intrinsic_needs_no_helpers() {
        // M10: `add.call(null, 1, 2, 3)` with fixed arguments — the direct
        // args are already contiguous on the working stack, so the fast path
        // is pure machine code: no `apply_args_fill` (no array) and no
        // `call_apply` round trip.
        static FILL_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static SLOW_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_apply_args_fill(ctx: *mut c_void, arg_array: u64, dest: u64) -> u64 {
            FILL_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.apply_args_fill)(ctx, arg_array, dest)
        }
        extern "C" fn counting_call_apply(
            ctx: *mut c_void,
            resolved: u64,
            callee: u64,
            argc: u64,
            args: *mut u64,
            kind: u64,
        ) -> u64 {
            SLOW_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.call_apply)(ctx, resolved, callee, argc, args, kind)
        }
        let mut helpers = runtime_helpers();
        helpers.apply_args_fill = Some(counting_apply_args_fill);
        helpers.call_apply = Some(counting_call_apply);
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
                "function add(a, b, c) { return a + b + c; }\n\
                 function run() {\n\
                   var s = 0;\n\
                   for (var i = 0; i < 1000; i++) { s += add.call(null, 1, 2, 3); }\n\
                   return s;\n\
                 }\n\
                 run();",
            )
            .expect("runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value.as_number(), Some(6000.0));
        assert!(compiled >= 2, "{compiled} bodies (run + add) must compile");
        let fills = FILL_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(fills, 0, "a fixed-arg `.call` copies no array elements");
        let slows = SLOW_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(slows, 0, "the intrinsic fast path never hits `call_apply`");
    }

    #[test]
    fn installed_jit_shadowed_apply_falls_back_to_the_slow_path() {
        // M10: an own `apply` property shadows the intrinsic — the compiled
        // site's identity check fails every iteration and the `call_apply`
        // slow path runs the resolved function with the original argument
        // list (`this` = the receiver `o`, args = `[null, arr]`).
        static FILL_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static SLOW_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_apply_args_fill(ctx: *mut c_void, arg_array: u64, dest: u64) -> u64 {
            FILL_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.apply_args_fill)(ctx, arg_array, dest)
        }
        extern "C" fn counting_call_apply(
            ctx: *mut c_void,
            resolved: u64,
            callee: u64,
            argc: u64,
            args: *mut u64,
            kind: u64,
        ) -> u64 {
            SLOW_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.call_apply)(ctx, resolved, callee, argc, args, kind)
        }
        let mut helpers = runtime_helpers();
        helpers.apply_args_fill = Some(counting_apply_args_fill);
        helpers.call_apply = Some(counting_call_apply);
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
                "var o = { apply: function (t, a) { return 42; } };\n\
                 function run() {\n\
                   var s = 0; var arr = [1, 2, 3];\n\
                   for (var i = 0; i < 1000; i++) { s += o.apply(null, arr); }\n\
                   return s;\n\
                 }\n\
                 run();",
            )
            .expect("runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value.as_number(), Some(42000.0));
        assert!(compiled >= 1, "{compiled} bodies (run) must compile");
        let slows = SLOW_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            slows, 1000,
            "a shadowed apply runs `call_apply` per iteration"
        );
        let fills = FILL_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(fills, 0, "a shadowed apply copies no dense elements");
    }

    #[test]
    fn installed_jit_non_function_call_receiver_keeps_the_type_error() {
        // M10: the fast path is gated on the receiver being a Function — a
        // plain object with %Function.prototype% in its chain resolves the
        // intrinsic `call` but is not callable, so the site must fall to
        // `do_call_apply`'s exact TypeError (not the ordinary call's
        // "is not a function" message).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var o = {}; Object.setPrototypeOf(o, Function.prototype);\n\
                     function run() {\n\
                       var message = '';\n\
                       for (var i = 0; i < 5000; i++) {\n\
                         try { o.call(1, 2); } catch (e) { message = e.message; }\n\
                       }\n\
                       return message === 'Call must be called on a function' ? 1 : 0;\n\
                     }\n\
                     run();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 1, "{compiled} bodies (run) must compile");
    }

    #[test]
    fn installed_jit_caught_call_error_in_a_loop_does_not_drift_the_working_sp() {
        // Cut 70: a compiled body whose loop throws a call error into its
        // OWN catch every iteration used to leave the erroring step's
        // operands on the working stack — the machine sp drifted +operands
        // per iteration until it overran the fixed buffer and corrupted the
        // ctx (the "a pending JIT error is present" panic after ~100-200
        // iterations; reproduced at HEAD). The catch entry now resumes at
        // the handler's try-entry sp. 100K caught throws must run clean.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function g() { throw 'x'; }\n\
                     function run() {\n\
                       var message = '';\n\
                       for (var i = 0; i < 100000; i++) {\n\
                         try { g(); } catch (e) { message = e; }\n\
                       }\n\
                       return message === 'x' ? 1 : 0;\n\
                     }\n\
                     run();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 2, "{compiled} bodies (run + g) must compile");
    }

    #[test]
    fn installed_jit_finally_continue_swallowing_loop_errors_does_not_drift() {
        // Cut 70: the finally-entry reset — a `continue` in a finally
        // overrides the per-iteration throw (spec 14.15.4 step 8), so the
        // loop runs with an error routed through the finally every
        // iteration. Without the reset the erroring operands drifted the sp
        // the same way the catch shape does.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function g() { throw 'x'; }\n\
                     function run() {\n\
                       var count = 0;\n\
                       for (var i = 0; i < 100000; i++) {\n\
                         try { g(); } finally { count++; continue; }\n\
                       }\n\
                       return count === 100000 ? 1 : 0;\n\
                     }\n\
                     run();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 2, "{compiled} bodies (run + g) must compile");
    }

    #[test]
    fn installed_jit_certified_callee_inside_caller_control_state_matches_the_interpreter() {
        // C1c: the certified-callee lane now runs inside the caller's
        // `try`/for-of/block instead of refusing the call (C1b's
        // `can_inline_leaf` gate fell back to the funnel whenever any control
        // stack was non-empty). The lane runs on the caller's `Vm`, so the
        // caller's control state must survive the nested run: the callee throws
        // inside the caller's `try` (the throw must route to the caller's
        // handler, not be swallowed), the caller's for-of keeps iterating after
        // the thrown element, a destructuring default calls the lane mid-
        // destructure, and a block binding read AFTER a nested run must still
        // see its value. Differential against the interpreter, with the folded
        // answer asserted so a shared wrong value cannot hide.
        let source = "function h(x) { return x + 1; }\n\
                      function g(x) { return h(x) + 1; }\n\
                      function run() {\n\
                        var out = 0;\n\
                        var a = [10, 20];\n\
                        for (var i = 0; i < 100; i++) {\n\
                          try {\n\
                            let k = i & 1;\n\
                            for (const v of a) {\n\
                              if (k === 1 && v === 20) { throw v; }\n\
                              out += g(v);\n\
                            }\n\
                          } catch (e) {\n\
                            out += e;\n\
                          }\n\
                        }\n\
                        var s = 0;\n\
                        for (var j = 0; j < 50; j++) {\n\
                          var [p = g(j), q = g(j + 1)] = [];\n\
                          s += g(p) - g(q);\n\
                        }\n\
                        var t = 0;\n\
                        for (var m = 0; m < 50; m++) {\n\
                          {\n\
                            let x = m;\n\
                            t += g(x);\n\
                            t += x;\n\
                          }\n\
                        }\n\
                        return (out === 3300 && s === -50 && t === 2550) ? 1 : 0;\n\
                      }\n\
                      run();";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the lane inside caller control state must match the interpreter"
        );
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 2, "{compiled} bodies (run + g) must compile");
    }

    #[test]
    fn installed_jit_a_certified_construct_matches_the_interpreter() {
        // C2: a call to a certified base constructor runs its compiled body as
        // a nested frame on the caller's `Vm` (the construct mirror of the
        // certified-callee lane): `construct_this_object` for the receiver, the
        // `this` slot, `current_new_target`, and the base-return rule. A
        // NON-leaf body (one that calls functions) now takes the lane too. The
        // shapes that must NOT (a derived `super()` constructor, a class with
        // instance fields) fall back to the general machinery and must still be
        // exact. Differential against the interpreter, with absolutes asserted
        // so a shared wrong value cannot hide.
        let programs = [
            // A non-leaf base constructor calling a function.
            "function h(x) { return x + 1; }\n\
             function Item(x) { this.x = h(x); this.y = x + 2; }\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) { s += new Item(i).x; } return s; }\n\
             bench();",
            // The base-return rule: an object return wins over the receiver.
            "function C(x) { this.x = x; return { tag: x + 1 }; }\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) { s += new C(i).tag; } return s; }\n\
             bench();",
            // The base-return rule: a primitive return falls back to the
            // receiver.
            "function C(x) { this.x = x; return 999; }\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) { s += new C(i).x; } return s; }\n\
             bench();",
            // A base class with no instance fields (a class constructor runs
            // the lane).
            "function h(x) { return x * 2; }\n\
             class A { constructor(x) { this.x = h(x); } }\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) { s += new A(i).x; } return s; }\n\
             bench();",
            // A derived constructor (`super`) must fall back and stay exact.
            "class B { constructor(x) { this.x = x; } }\n\
             class D extends B { constructor(x) { super(x + 1); } }\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) { s += new D(i).x; } return s; }\n\
             bench();",
            // A class with instance fields must fall back and stay exact.
            "function h(x) { return x + 1; }\n\
             class F { y = h(5); constructor(x) { this.x = x; } }\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) { var o = new F(i); s += o.x + o.y; } return s; }\n\
             bench();",
        ];
        for source in programs {
            let (jit_value, compiled) = run_self_call_program(source, true);
            let (interp_value, _) = run_self_call_program(source, false);
            assert_eq!(
                jit_value, interp_value,
                "jit and interpreter disagree:\n{source}"
            );
            assert!(compiled >= 1, "{compiled} bodies must compile:\n{source}");
        }
        // Absolutes, independent of any compiled run.
        assert_eq!(run_self_call_program(programs[0], true).0, 5050.0);
        assert_eq!(run_self_call_program(programs[0], false).0, 5050.0);
        assert_eq!(run_self_call_program(programs[1], true).0, 5050.0);
        assert_eq!(run_self_call_program(programs[2], true).0, 4950.0);
        assert_eq!(run_self_call_program(programs[3], true).0, 9900.0);
        assert_eq!(run_self_call_program(programs[4], true).0, 5050.0);
        assert_eq!(run_self_call_program(programs[5], true).0, 5550.0);
    }

    #[test]
    fn installed_jit_apply_nullish_arraylike_and_empty_shapes() {
        // M10: the intrinsic fast path's arg-list shapes — a nullish
        // argArray (0 args), an empty dense array (0 args), and an
        // array-like object (the `create_list_from_array_like` slow path),
        // plus a no-argument `.call()` (this defaults to undefined).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function first(x, y) { return y === undefined ? 41 : x; }\n\
                     function run() {\n\
                       var s = 0;\n\
                       for (var i = 0; i < 100; i++) {\n\
                         s += first.apply(null);\n\
                         s += first.apply(null, []);\n\
                         s += first.apply(null, { length: 2, 0: 7, 1: 8 });\n\
                         s += first.call(null, 9, 10);\n\
                         s += first.call();\n\
                       }\n\
                       return s;\n\
                     }\n\
                     run();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(13900.0));
        assert!(
            compiled >= 2,
            "{compiled} bodies (run + first) must compile"
        );
    }

    #[test]
    fn installed_jit_stale_epoch_leaf_cache_revalidates_at_rest() {
        // Cut 68: a monomorphic hot leaf call next to a disturbing helper
        // (the `o.g` getter bumps the leaf-eligibility epoch on every
        // iteration) must NOT re-probe every iteration. The counting probe
        // wrapper proves it. `add` is a straight-line body, so the loop's call
        // site is refused while the compile threshold defers it — and that
        // refusal is deliberately NOT cached (it is transient; see
        // `installed_jit_a_deferred_leaf_call_site_stops_using_call_slow`), so
        // the site probes a few times during the tier-up window rather than
        // once. Once `add` compiles the cached verdict is reused across the
        // epoch bumps: each later visit finds a stale epoch, re-validates that
        // the eligibility state is still at rest, and reuses it — so the count
        // stays in the single digits (`JIT_COMPILE_THRESHOLD` is 16, and the
        // `call_slow` lane advances the same counter, so the measured count is
        // 9). A per-iteration re-probe would count ~100K; the bound admits the
        // tier-up window and still fails that.
        static PROBE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_leaf_call_probe(
            ctx: *mut c_void,
            callee: u64,
            this: u64,
            args: *mut u64,
            argc: u64,
            site: u64,
        ) -> u64 {
            PROBE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.leaf_call_probe)(ctx, callee, this, args, argc, site)
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
            (1..=20).contains(&probes),
            "the leaf site must probe only during the tier-up window and then reuse its verdict ({probes} total): a per-iteration re-probe would count ~100K"
        );
    }

    #[test]
    fn installed_jit_two_hot_leaf_sites_each_cache_separately() {
        // Cut 68: TWO hot leaf call sites in one loop body used to alternate
        // on a single record — every visit missed and re-probed (~200K probes
        // for a 100K-iteration loop). G15 keyed the agent's record table by
        // the CALLEE, so two sites with different callees never collide at
        // all (and two sites with the same callee share one warm verdict), so
        // each site warms once and the loop reuses both. The counting probe
        // wrapper proves it: each site probes only during `add`'s tier-up
        // window (a deferred refusal is not cached by design — measured 10
        // total, bound admits both windows), and the ~200K remaining visits
        // reuse the two cached verdicts. A per-iteration re-probe would count
        // ~200K.
        static PROBE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_leaf_call_probe(
            ctx: *mut c_void,
            callee: u64,
            this: u64,
            args: *mut u64,
            argc: u64,
            site: u64,
        ) -> u64 {
            PROBE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.leaf_call_probe)(ctx, callee, this, args, argc, site)
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
            (2..=40).contains(&probes),
            "each hot site must probe only during the tier-up window and then reuse its verdict ({probes} total): alternating single-record misses would count ~200K"
        );
    }

    #[test]
    fn installed_jit_a_deferred_leaf_call_site_stops_using_call_slow() {
        // G2: a straight-line leaf is deferred by the compile threshold, so a
        // compiled loop's call site is refused on its first visits. That
        // refusal is transient — the consult counts toward the threshold — so
        // the site must re-probe and then inline instead of caching "not
        // inlineable" for the whole loop, which is what left every
        // straight-line leaf call on `call_slow` (measured on `direct_leaf`:
        // 2,000,000 `call_slow` calls before, 8 after). The counting wrapper
        // proves it: the site reaches `call_slow` only during the tier-up
        // window, not for all 100K iterations.
        static CALL_SLOW_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_call_slow(
            ctx: *mut c_void,
            callee: u64,
            this: u64,
            argc: u64,
            args: *mut u64,
            direct_eval: u64,
        ) -> u64 {
            CALL_SLOW_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.call_slow)(ctx, callee, this, argc, args, direct_eval)
        }
        let mut helpers = runtime_helpers();
        helpers.call_slow = Some(counting_call_slow);

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
                   for (var i = 0; i < 100000; i++) { s += add(i, 1); }\n\
                   return s;\n\
                 }\n\
                 run();",
            )
            .expect("runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value.as_number(), Some(5000050000.0));
        assert!(compiled >= 2, "{compiled} bodies (run + add) must compile");
        let slow = CALL_SLOW_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            slow <= 40,
            "the site must inline after its tier-up window ({slow} call_slow calls): caching the transient refusal would count ~100K"
        );
    }

    #[test]
    fn installed_jit_a_self_recursion_with_a_tail_base_matches_the_interpreter() {
        // Hazard probe for the shared-Vm self path: a strict self-eligible
        // body whose base case is a *tail call* emits `Helper::TailCall`, which
        // calls `tail_prepare_ordinary` on the Vm and sets `ctx.tail`. In the
        // nested (self-inlined) run the Vm and ctx are the caller's, so if the
        // self path does not account for that, the outer body resumes on a Vm
        // reset for another body and computes a wrong value.
        let source = "\"use strict\";\n\
            function g(x) { return x + 100; }\n\
            function bench() {\n\
              function f(n) { if (n <= 0) return g(7); return f(n - 1) + 1; }\n\
              var s = 0;\n\
              for (var i = 0; i < 100; i++) s += f(5);\n\
              return s;\n\
            }\n\
            bench();";
        let (jit_value, _) = run_self_call_program(source, true);
        let (interp_value, _) = run_self_call_program(source, false);
        assert_eq!(jit_value, interp_value, "jit vs interpreter:\n{source}");
        assert_eq!(jit_value, 11200.0);
    }

    #[test]
    fn installed_jit_self_call_hazards_match_the_interpreter() {
        // Shapes whose steps touch caller-owned Vm state the nested self run
        // would otherwise share: the argument vector (a spread call), the
        // string builder, and the statement-completion register (a
        // statement-position fused call-store). `ArgsSpread`/`Call`/
        // `TaggedTemplate` are gated out of self-call eligibility; the builder
        // and completion register are not, so this test is what keeps them
        // honest.
        let shapes: &[(&str, f64)] = &[
            (
                "function h() { var t = 0; for (var i = 0; i < arguments.length; i++) t += arguments[i]; return t; }\n\
                 function bench() {\n\
                   function f(n) { var a = [1, 2, 3]; if (n <= 0) return h(...a); return f(n - 1) + 1; }\n\
                   var s = 0;\n\
                   for (var i = 0; i < 100; i++) s += f(5);\n\
                   return s;\n\
                 }\n\
                 bench();",
                1100.0,
            ),
            (
                "function bench() {\n\
                   function f(n) { var s = \"\"; for (var i = 0; i < 3; i++) s += n; if (n <= 0) return s; return f(n - 1) + s; }\n\
                   var out = 0;\n\
                   for (var i = 0; i < 100; i++) out += f(3).length;\n\
                   return out;\n\
                 }\n\
                 bench();",
                1200.0,
            ),
            (
                "function bench() {\n\
                   function f(n) { var r; if (n <= 0) return 0; r = f(n - 1); return r + 1; }\n\
                   var s = 0;\n\
                   for (var i = 0; i < 100; i++) s += f(5);\n\
                   return s;\n\
                 }\n\
                 bench();",
                500.0,
            ),
        ];
        for (source, expected) in shapes {
            let (jit_value, _) = run_self_call_program(source, true);
            let (interp_value, _) = run_self_call_program(source, false);
            assert_eq!(jit_value, interp_value, "jit vs interpreter:\n{source}");
            assert_eq!(jit_value, *expected, "absolute:\n{source}");
        }
    }

    /// Run `source` to completion, installing a fresh JIT cache when `jit` is
    /// set, and return the script's numeric result and the compiled-body
    /// count.
    fn run_self_call_program(source: &str, jit: bool) -> (f64, usize) {
        extern "C" fn noop_drop(_cache: *mut c_void) {}
        let mut agent = runtime::Agent::new();
        agent.initialize_host_defined_realm().expect("realm");
        let mut cache = JitCache::new(runtime_helpers()).expect("isa");
        if jit {
            agent.jit_hook = Some(runtime::jit::JitHook {
                cache: (&mut cache as *mut JitCache) as *mut c_void,
                lookup: jit_cache_lookup,
                drop_cache: noop_drop,
                helpers: &runtime::jit::JIT_SLOW_PATHS,
            });
        }
        let value = agent.run_script(source).expect("runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        (value.as_number().expect("a number"), compiled)
    }

    #[test]
    fn installed_jit_a_self_recursive_call_matches_the_interpreter() {
        // G21: a self-recursive function's non-tail call site goes through
        // `call_slow`, which runs the callee's compiled body directly (the
        // "compiled self-call") instead of re-entering the interpreter's call
        // machinery. The path is JIT-only, so a differential against the
        // interpreter (no hook) is the exercise: a wrong inline frame fill or
        // control mistake shows up as a different result, and neutralizing
        // the fill makes this test fail (a shared bug cannot hide because the
        // literal answers below are asserted too). Each program is hot enough
        // that `compiled >= 2`, so the recursive body really compiled.
        let programs = [
            // Argument passing through the inline frame (both params).
            "function bench() {\n\
               function sum(n, acc) { if (n <= 0) return acc; return sum(n - 1, acc + n * 2 + 1); }\n\
               var s = 0;\n\
               for (var i = 0; i < 100; i++) s += sum(6, 0);\n\
               return s;\n\
             }\n\
             bench();",
            // The corpus shape: a nested `fib` reading its own name from the
            // enclosing context (`LoadContext`), under a hot outer loop.
            "function bench() {\n\
               function fib(n) { if (n < 2) return n; return fib(n - 1) + fib(n - 2); }\n\
               var s = 0;\n\
               for (var i = 0; i < 400; i++) { s += fib(i % 12); }\n\
               return s;\n\
             }\n\
             bench();",
            // A `var` slot past the params, reset at every inline entry.
            "function bench() {\n\
               function f(n) { var v; if (v !== undefined) return -1; v = 1; if (n <= 0) return 0; return 1 + f(n - 1); }\n\
               var s = 0;\n\
               for (var i = 0; i < 100; i++) s += f(8);\n\
               return s;\n\
             }\n\
             bench();",
        ];
        for source in programs {
            let (jit_value, compiled) = run_self_call_program(source, true);
            let (interp_value, _) = run_self_call_program(source, false);
            assert_eq!(
                jit_value, interp_value,
                "jit and interpreter disagree:\n{source}"
            );
            assert!(
                compiled >= 2,
                "the recursive body must compile so the self path is reachable ({compiled} bodies):\n{source}"
            );
        }
        // The absolute answers, independent of any compiled run (a wrong
        // param/vars frame fill in the self path yields a different number):
        assert_eq!(run_self_call_program(programs[0], true).0, 4800.0);
        assert_eq!(run_self_call_program(programs[0], false).0, 4800.0);
        assert_eq!(run_self_call_program(programs[1], true).0, 7660.0);
        assert_eq!(run_self_call_program(programs[2], true).0, 800.0);
    }

    #[test]
    fn installed_jit_a_certified_callee_matches_the_interpreter() {
        // Stage 9 Cut 1: a call to a *different* certified body runs its machine
        // code directly on the caller's ctx (the certified-callee lane) instead
        // of the interpreter funnel. The lane shares the caller's `Vm`, so this
        // is where the scratch-register isolation (`switch_disc`,
        // `chain_short`, the builder, `ip`), the per-callee `globals_unshadowed`,
        // and the nested frame a frame-reading helper must address
        // (`Vm::nested_frame`, for a builder or function-declaration callee),
        // and the `this` binding the lane performs for a method callee are
        // exercised; a differential against the interpreter (no hook) is the
        // check, with the absolute answers asserted too so a shared wrong value
        // cannot hide.
        let programs = [
            // A callee's `switch` must not disturb the caller's fall-through
            // switch discriminant (both live in `Vm::switch_disc`).
            "function callee(x) { switch (x) { case 1: return 10; case 2: return 20; default: return 30; } }\n\
             function bench() {\n\
               var out = 0;\n\
               for (var i = 0; i < 100; i++) {\n\
                 switch (i & 1) {\n\
                   case 0: out += callee(2);\n\
                   case 1: out += callee(1);\n\
                 }\n\
               }\n\
               return out;\n\
             }\n\
             bench();",
            // Mutual recursion: every call is a different-body call, so the
            // lane nests through `call_slow` at every level.
            "function isEven(n) { if (n === 0) return 1; var r = isOdd(n - 1); return r; }\n\
             function isOdd(n) { if (n === 0) return 0; var r = isEven(n - 1); return r; }\n\
             function bench() { var c = 0; for (var i = 0; i < 100; i++) c += isEven(10); return c; }\n\
             bench();",
            // A callee reading a global: its `globals_unshadowed` must be
            // computed for its own names, not the caller's.
            "var g = 7;\n\
             function addg(x) { return x + g; }\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) s += addg(i); return s; }\n\
             bench();",
            // A callee with a planned `s += e` append loop: its builder helper
            // reads frame slots through `Vm::frame_get`, which the lane points
            // at the nested frame (`Vm::nested_frame`) — without that it would
            // address the caller's frame and return the wrong string.
            "function build(n) { var s = ''; for (var i = 0; i < n; i++) s += 'x'; return s.length; }\n\
             function bench() { var out = 0; for (var i = 0; i < 100; i++) out += build(5); return out; }\n\
             bench();",
            // A callee with a hoisted block function declaration: its slot
            // store also goes through `Vm::frame_get_mut` (same requirement).
            "function outer() { function inner() { return 7; } return inner(); }\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) s += outer(); return s; }\n\
             bench();",
            // A callee that reads `this`: the lane binds OrdinaryCallBindThis
            // into the frame's `this` slot (sloppy nullish -> the realm
            // global).
            "function probe() { return this === globalThis ? 1 : 0; }\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) s += probe(); return s; }\n\
             bench();",
            // A method (non-leaf, so the leaf probe cannot take it): `this` is
            // the receiver.
            "function add(a, b) { return a + b; }\n\
             var o = { x: 5, m(n) { return add(this.x, n); } };\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) s += o.m(i); return s; }\n\
             bench();",
            // A `super` method must fall back: the lane installs no home
            // object or this-binding for the super machinery.
            "class B { m() { return 1; } }\n\
             class D extends B { m() { return super.m() + 1; } }\n\
             var d = new D();\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) s += d.m(); return s; }\n\
             bench();",
            // A strict body observing `arguments`: the unmapped object reads
            // `Vm::call_args`, which the lane sets for the run.
            "function f(a, b) { 'use strict'; return arguments.length + a + b; }\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) s += f(i, 2); return s; }\n\
             bench();",
            // The sloppy mapped form must fall back: its accessors alias the
            // callee's parameter environment, which the lane does not build.
            "function g(a) { a = 5; return arguments[0]; }\n\
             function bench() { var s = 0; for (var i = 0; i < 100; i++) s += g(i); return s; }\n\
             bench();",
        ];
        for source in programs {
            let (jit_value, compiled) = run_self_call_program(source, true);
            let (interp_value, _) = run_self_call_program(source, false);
            assert_eq!(jit_value, interp_value, "jit vs interpreter:\n{source}");
            assert!(
                compiled >= 2,
                "the callee must compile so the lane is reachable ({compiled} bodies):\n{source}"
            );
        }
        assert_eq!(run_self_call_program(programs[0], true).0, 2000.0);
        assert_eq!(run_self_call_program(programs[0], false).0, 2000.0);
        assert_eq!(run_self_call_program(programs[1], true).0, 100.0);
        assert_eq!(run_self_call_program(programs[2], true).0, 5650.0);
        assert_eq!(run_self_call_program(programs[3], true).0, 500.0);
        assert_eq!(run_self_call_program(programs[3], false).0, 500.0);
        assert_eq!(run_self_call_program(programs[4], true).0, 700.0);
        assert_eq!(run_self_call_program(programs[5], true).0, 100.0);
        assert_eq!(run_self_call_program(programs[5], false).0, 100.0);
        assert_eq!(run_self_call_program(programs[6], true).0, 5450.0);
        assert_eq!(run_self_call_program(programs[7], true).0, 200.0);
        assert_eq!(run_self_call_program(programs[7], false).0, 200.0);
        assert_eq!(run_self_call_program(programs[8], true).0, 5350.0);
        assert_eq!(run_self_call_program(programs[8], false).0, 5350.0);
        assert_eq!(run_self_call_program(programs[9], true).0, 500.0);
        assert_eq!(run_self_call_program(programs[9], false).0, 500.0);
    }

    #[test]
    fn installed_jit_a_deep_self_recursion_falls_back_past_the_depth_cap() {
        // Past `MAX_JIT_DEPTH` the self path declines to the interpreter
        // (`run_jit_body` refuses a deeper compiled frame), so a recursion
        // beyond the cap must still compute correctly, and a runaway one must
        // still surface the interpreter's catchable stack-exhaustion guard
        // rather than bypass it. The recursion is deliberately non-tail (the
        // tail form would take `TailCallSelfCheck` and never reach the self
        // path). The debug interpreter spends a lot of native stack per
        // activation, so the default test stack cannot host many levels — the
        // correct-computation case runs on a deliberately large stack.
        let below_limit = "function bench() {\n\
            function down(n, acc) { if (n <= 0) return acc; var r = down(n - 1, acc + 1); return r + 1; }\n\
            return down(400, 0);\n\
          }\n\
          bench();";
        let (jit_value, interp_value) = std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                (
                    run_self_call_program(below_limit, true).0,
                    run_self_call_program(below_limit, false).0,
                )
            })
            .expect("spawn")
            .join()
            .expect("joins");
        assert_eq!(jit_value, 800.0);
        assert_eq!(interp_value, 800.0);
        // A runaway recursion hits the interpreter's guard (the self path caps
        // at `MAX_JIT_DEPTH`, which is far below the exhaustion point): the
        // error is catchable and identical with and without the JIT.
        let runaway = "function bench() {\n\
            function down(n, acc) { if (n <= 0) return acc; var r = down(n - 1, acc + 1); return r + 1; }\n\
            try { down(1000000, 0); return 0; } catch (e) { return e instanceof RangeError ? 1 : 2; }\n\
          }\n\
          bench();";
        assert_eq!(run_self_call_program(runaway, true).0, 1.0);
        assert_eq!(run_self_call_program(runaway, false).0, 1.0);
    }

    #[test]
    fn installed_jit_a_this_slot_leaf_call_inlines() {
        // G12: a leaf whose body reads `this` lowers the read to
        // `LoadLocal { this_slot }` (a frame read, leaf-eligible), so the body
        // IS leaf-certified — but the probe refused it outright ("the machine
        // code cannot bind `this`"), which left every method call on
        // `call_slow`. The probe now takes the call's receiver, applies
        // `OrdinaryCallBindThis`, and fills the frame's `this` slot. A sloppy
        // primitive receiver boxes (an allocation the helper must do), so it
        // still refuses; an object receiver — the method case — inlines.
        static CALL_SLOW_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_call_slow(
            ctx: *mut c_void,
            callee: u64,
            this: u64,
            argc: u64,
            args: *mut u64,
            direct_eval: u64,
        ) -> u64 {
            CALL_SLOW_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.call_slow)(ctx, callee, this, argc, args, direct_eval)
        }
        let mut helpers = runtime_helpers();
        helpers.call_slow = Some(counting_call_slow);

        let source = "var g = this;\n\
                      var o = { v: 1, m: function (x) { this.v += x; return this.v; } };\n\
                      function who() { return this; }\n\
                      function run() {\n\
                        var s = 0;\n\
                        for (var i = 0; i < 100000; i++) {\n\
                          s += o.m(1);\n\
                          if (who() === g) { s += 1; }\n\
                        }\n\
                        return s;\n\
                      }\n\
                      run();";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(
            value, interp,
            "the `this` binding must match the interpreter"
        );
        assert!(
            compiled >= 2,
            "{compiled} bodies (run + the methods) must compile"
        );
        let slow = CALL_SLOW_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        // The method's `this.v += x` store reads the receiver, and `who`
        // returns its bound `this` (the realm global for a sloppy plain call).
        // A body that reads `this` alongside an identifier read is NOT
        // leaf-certified (`LoadIdent` is leaf-excluded), and a strict body's
        // `return this` refuses for a reason upstream of this probe — both
        // keep their `call_slow` fallback, so this test covers the shapes the
        // probe now accepts.
        assert!(
            slow <= 60,
            "the method and `this`-reading sites must inline after their tier-up windows ({slow} call_slow calls): refusing a `this`-slot leaf would count ~200K"
        );
    }

    #[test]
    fn installed_jit_a_non_aliased_leaf_call_fills_without_re_probing() {
        // G14: a leaf whose frame is NOT the argument region (any `var`/
        // lexical slot past the params, or a `this` slot) must have its frame
        // rebuilt above the arguments on every call. That used to re-run the
        // whole `leaf_call_probe` per visit (its eligibility checks, its
        // `lookup_info` consult and its cache-record rewrite); the compiled
        // hit path now rebuilds the frame through `leaf_call_fill` instead, so
        // the probe runs only while the site warms up. Counting the two
        // helpers isolates the fix: the reverted per-visit probe counts ~100K.
        static PROBE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static FILL_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_leaf_call_probe(
            ctx: *mut c_void,
            callee: u64,
            this: u64,
            args: *mut u64,
            argc: u64,
            site: u64,
        ) -> u64 {
            PROBE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.leaf_call_probe)(ctx, callee, this, args, argc, site)
        }
        extern "C" fn counting_leaf_call_fill(
            ctx: *mut c_void,
            callee: u64,
            this: u64,
            args: *mut u64,
            argc: u64,
            site: u64,
        ) -> u64 {
            FILL_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.leaf_call_fill)(ctx, callee, this, args, argc, site)
        }
        let mut helpers = runtime_helpers();
        helpers.leaf_call_probe = Some(counting_leaf_call_probe);
        helpers.leaf_call_fill = Some(counting_leaf_call_fill);

        // `f` has a `var` past its one parameter, so its frame (2 slots) is not
        // the argument region (1 slot): every call needs a fill.
        let source = "function f(x) { var t = 2; return x + t; }\n\
                      function run() {\n\
                        var s = 0;\n\
                        for (var i = 0; i < 100000; i++) { s += f(i); }\n\
                        return s;\n\
                      }\n\
                      run();";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(
            value, interp,
            "the built frame must bind exactly as the interpreter's"
        );
        assert!(compiled >= 2, "{compiled} bodies must compile");
        let probes = PROBE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        let fills = FILL_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            fills >= 90_000,
            "the warm site must fill every call through `leaf_call_fill` ({fills} fills)"
        );
        assert!(
            probes <= 200,
            "the probe must run only while the site warms up ({probes} probes): a per-visit re-probe would count ~100K"
        );
    }

    #[test]
    fn the_emitter_and_runtime_leaf_record_slots_agree() {
        // G15: the compiled code computes the record slot in
        // `emit_leaf_record_slot`; every runtime writer computes it in
        // `runtime::jit::leaf_record_slot`. If they drift, a verdict is written
        // to one slot and read from another — the site silently never inlines.
        // Keep the two expressions byte-for-byte equivalent.
        fn emitter(callee: u64) -> usize {
            (callee.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> runtime::jit::LEAF_CALL_RECORD_SHIFT)
                as usize
        }
        for callee in [
            0u64,
            0x7ff8_0000_0000_0000,
            0x7ff8_0000_1234_5670,
            0x7ff8_0000_9abc_def0,
            0x7ff8_0001_0000_0000,
            0x0fff_ffff_ffff_ffff,
        ] {
            assert_eq!(
                emitter(callee),
                runtime::jit::leaf_record_slot(callee),
                "callee={callee:#x}"
            );
        }
        // Every slot must be reachable (a dead slot would waste a record and
        // leave its callees colliding forever). A direct-mapped table cannot
        // avoid occasional collisions among arbitrary callees, so this checks
        // coverage, not perfection.
        let slots: std::collections::HashSet<usize> = (0..4096u64)
            .map(|i| runtime::jit::leaf_record_slot(0x7ff8_0000_1000_0000 + i))
            .collect();
        assert_eq!(
            slots.len(),
            runtime::jit::LEAF_CALL_RECORDS,
            "every slot must be reachable"
        );
    }

    #[test]
    fn the_emitter_and_runtime_computed_read_slots_agree() {
        // G8: the compiled computed-read probe computes its slot inline in
        // `emit_element_read` (`emit_computed_read_slot`); every runtime access
        // computes it in `runtime::ir::computed_read_cell_index`. If the two
        // drift, the probe reads a slot the runtime never writes and the read
        // silently never inlines. Keep the two expressions byte-for-byte
        // equivalent.
        fn emitter(key_bits: u64) -> usize {
            (key_bits.wrapping_mul(runtime::ir::COMPUTED_READ_INDEX_MUL)
                >> runtime::ir::COMPUTED_READ_INDEX_SHIFT) as usize
                & (runtime::ir::COMPUTED_READ_CELLS - 1)
        }
        for key_bits in [
            0u64,
            0x7ff8_0000_0000_0000,
            0x7ff8_0005_1234_5670,
            0x0fff_ffff_ffff_ffff,
        ] {
            assert_eq!(
                emitter(key_bits),
                runtime::ir::computed_read_cell_index(key_bits),
                "key={key_bits:#x}"
            );
        }
        // Every slot must be reachable (a dead slot would waste a key forever).
        let slots: std::collections::HashSet<usize> = (0..4096u64)
            .map(|i| runtime::ir::computed_read_cell_index(0x7ff8_0000_1000_0000 + i))
            .collect();
        assert_eq!(
            slots.len(),
            runtime::ir::COMPUTED_READ_CELLS,
            "every slot must be reachable"
        );
    }

    #[test]
    fn installed_jit_an_env_leaf_call_runs_through_the_env_lane() {
        // G13: a leaf that reads a captured binding lowers the read to
        // `LoadContextSlot`, which is leaf-safe — the interpreter's own leaf
        // gate has no env condition. The compiled call site cannot run it
        // in-frame (the body_context swap has to span the call), so the site
        // takes the env lane, which performs the whole call from the record.
        // Counting `call_slow` at the site isolates the fix.
        static CALL_SLOW_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_call_slow(
            ctx: *mut c_void,
            callee: u64,
            this: u64,
            argc: u64,
            args: *mut u64,
            direct_eval: u64,
        ) -> u64 {
            CALL_SLOW_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.call_slow)(ctx, callee, this, argc, args, direct_eval)
        }
        let mut helpers = runtime_helpers();
        helpers.call_slow = Some(counting_call_slow);

        // `inner` reads `base` through the enclosing function's capture
        // context, so it is an env leaf; the call site is monomorphic, so the
        // record's verdict holds.
        let source = "function run() {\n\
                        var base = 1;\n\
                        function inner(x) { return base + x; }\n\
                        var s = 0;\n\
                        for (var i = 0; i < 100000; i++) { s += inner(i); }\n\
                        return s;\n\
                      }\n\
                      run();";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(
            value, interp,
            "the captured read must match the interpreter"
        );
        assert!(
            compiled >= 2,
            "{compiled} bodies (run + inner) must compile"
        );
        let slow = CALL_SLOW_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            slow <= 60,
            "the env site must inline after its tier-up window ({slow} call_slow calls): refusing the env leaf would count ~100K"
        );
    }

    #[test]
    fn installed_jit_straight_line_body_stays_interpreted_below_the_threshold() {
        // Cut 69: a straight-line body (`add`, no loop) is run interpreted
        // until its consult count reaches `JIT_COMPILE_THRESHOLD`. Eight
        // calls stay below the threshold, and the consulted-once script body
        // is below it too — nothing compiles, and the behavior is identical.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function add(a, b) { return a + b; } \
                     add(1, 2); add(1, 2); add(1, 2); add(1, 2); \
                     add(1, 2); add(1, 2); add(1, 2); add(1, 2);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(3.0));
        assert_eq!(compiled, 0, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_straight_line_body_promotes_at_the_threshold() {
        // Cut 69: the 17th consult of a straight-line body crosses the
        // threshold and compiles it — `add` runs its machine code on the
        // last call (the script body, consulted once, stays interpreted).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function add(a, b) { return a + b; } \
                     add(1, 2); add(1, 2); add(1, 2); add(1, 2); add(1, 2); add(1, 2); \
                     add(1, 2); add(1, 2); add(1, 2); add(1, 2); add(1, 2); add(1, 2); \
                     add(1, 2); add(1, 2); add(1, 2); add(1, 2); add(1, 2);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(3.0));
        assert_eq!(compiled, 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_loop_body_compiles_on_the_first_consult() {
        // Cut 69: a loop body (`sum`, has a back edge) bypasses the
        // threshold — it runs once with many internal iterations, so a
        // consult count would never promote it. It compiles on the first
        // call, even though it is only ever called once.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function sum(n) { var s = 0; for (var i = 0; i <= n; i++) { s += i; } return s; } \
                     sum(100);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(5050.0));
        assert_eq!(compiled, 1, "{compiled} bodies");
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
        // would return the original's (1). Cut 69: f's body is a
        // self-tail-call (a loop shape) and compiles on first use; g's
        // tail call targets a different name, so g stays interpreted.
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
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_three_arg_call() {
        // Cut 49: a ≥3-argument call compiles through the vector form
        // (`ArgsBase`/`ArgsPush`/`Call`) — the certified script's call to
        // `f(1, 2, 3)` no longer bails the body to the interpreter. Cut 69:
        // `f` is straight-line, so the call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(a, b, c) { return a + b + c; } \
                     f(1, 2, 3); f(1, 2, 3); f(1, 2, 3); f(1, 2, 3); f(1, 2, 3); f(1, 2, 3); \
                     f(1, 2, 3); f(1, 2, 3); f(1, 2, 3); f(1, 2, 3); f(1, 2, 3); f(1, 2, 3); \
                     f(1, 2, 3); f(1, 2, 3); f(1, 2, 3); f(1, 2, 3); f(1, 2, 3);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(6.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_spread_call() {
        // Cut 49: `ArgsSpread` iterates the array into the argument vector.
        // Cut 69: `f` is straight-line, so the call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(a, b, c) { return a + b + c; } \
                     f(...[1, 2, 3]); f(...[1, 2, 3]); f(...[1, 2, 3]); f(...[1, 2, 3]); \
                     f(...[1, 2, 3]); f(...[1, 2, 3]); f(...[1, 2, 3]); f(...[1, 2, 3]); \
                     f(...[1, 2, 3]); f(...[1, 2, 3]); f(...[1, 2, 3]); f(...[1, 2, 3]); \
                     f(...[1, 2, 3]); f(...[1, 2, 3]); f(...[1, 2, 3]); f(...[1, 2, 3]); \
                     f(...[1, 2, 3]);",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(6.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_runs_a_vector_tail_call_chain() {
        // Cut 49/M2: a >`FAST_CALL_MAX_ARGS` (64) tail call compiles through
        // the vector `TailCall` — the 100K-deep chain runs with bounded
        // stack.
        let (value, compiled) = with_jit_agent(|agent| {
            agent.run_script(
                "\"use strict\"; function g(n, a, b, c, d, e, f, h, i, j, k, l, m, o, p, q, r, s, t, u, v, w, x, y, z, A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z, a0, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13) { if (n === 0) { return a + b + c + d + e + f + h + i + j + k + l + m + o + p + q + r + s + t + u + v + w + x + y + z + A + B + C + D + E + F + G + H + I + J + K + L + M + N + O + P + Q + R + S + T + U + V + W + X + Y + Z + a0 + a1 + a2 + a3 + a4 + a5 + a6 + a7 + a8 + a9 + a10 + a11 + a12 + a13; } return g(n - 1, a + 1, b, c, d, e, f, h, i, j, k, l, m, o, p, q, r, s, t, u, v, w, x, y, z, A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z, a0, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13); } g(100000, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63);",
            )
            .expect("runs")
        });
        assert_eq!(value.as_number(), Some(102016.0));
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
        // computed-callee tail call. Cut 69: the callee of a computed tail
        // call is not statically the running body, so `f` is not a loop body
        // and stays interpreted (consulted once); `getF` is consulted once
        // per TCO step and promotes — `count` lands once at the base case.
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
        assert!(compiled >= 1, "{compiled} bodies");
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
        // member read) routes through the same pending-error dispatch. Cut
        // 69: `f` is straight-line, so the call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { try { throw 42; } catch (e) { return e * 2; } }\n\
                     function g() { try { var x = null; return x.y.z; } catch (e) { return e.name; } }\n\
                     f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); \
                     f(); f(); f(); f(); f();",
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
        // through a finally routes via `control_transfer` too. Cut 69: `f`
        // is straight-line, so the call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var log = [];\n\
                     function f() { try { log.push('t'); return 1; } finally { log.push('f'); } }\n\
                     function g() { try { return 1; } finally { return 2; } }\n\
                     function h() { var out = ''; for (var i = 0; i < 3; i++) { \
                       try { if (i === 1) continue; out += i; } finally { out += 'f'; } } return out; }\n\
                     f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); \
                     f(); f(); f(); f(); f();",
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
        // value. Cut 69: `f` and `g` are straight-line, so the scenario
        // repeats 17× (the precomputed result string is returned untouched;
        // the script body itself is a loop, so it compiles too).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var log = [];\n\
                     function f() { try { try { throw 'inner'; } finally { log.push('f'); } } \
                       catch (e) { return e + '!'; } }\n\
                     function g() { try { throw 'escaped'; } finally { log.push('g'); } }\n\
                     var caught = null;\n\
                     try { g(); } catch (e) { caught = e; }\n\
                     var result = f() + '|' + caught + '|' + log.join(',');\n\
                     var i = 0;\n\
                     while (i++ < 16) { try { g(); } catch (e) {} f(); }\n\
                     result;",
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
        // stack balanced for the leaf-probe eligibility checks). Cut 69:
        // `f` is straight-line, so the call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { { let x = 5; var y = x * 2; } return y; }\n\
                     f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); \
                     f(); f(); f(); f(); f();",
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
                     f(1); f(1); f(1); f(1); f(1); f(1); f(1); f(1); f(1); f(1); f(1); \
                     f(1); f(1); f(1); f(1); f(1); f(1);",
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
        // Cut 69: the async body is consulted once per resume (below the
        // promotion threshold for a 2-await body), so `f` repeats 17× via
        // the script loop.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var result = 'pending';\n\
                     async function f(x) { var a = await x; var b = await (a + 1); return b * 2; }\n\
                     var i = 0;\n\
                     while (i++ < 17) { f(10).then(function (v) { result = v; }); }",
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
        // the resumed segment runs the catch in machine code. Cut 69: the
        // async body is consulted once per resume, so `f` repeats 17× via
        // the script loop.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var result = 'pending';\n\
                     async function f() { var log = ''; try { await Promise.reject('boom'); } \
                       catch (e) { log += 'c' + e; } return log + 'done'; }\n\
                     var i = 0;\n\
                     while (i++ < 17) { f().then(function (v) { result = v; }); }",
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
        // escapes the body and rejects the promise). Cut 69: the async body
        // is consulted once per resume, so `f` repeats 17× via the script
        // loop.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var result = 'pending';\n\
                     async function f() { var log = ''; try { await Promise.reject('x'); } \
                       finally { log += 'f'; } return log; }\n\
                     var i = 0;\n\
                     while (i++ < 17) { \
                       f().then(function (v) { result = 'ok:' + v; }, function (e) { result = 'rej:' + e; }); }",
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
        // `return` completes the iteration. Cut 69: a generator body is
        // consulted once per resume, so the first generator's output drives
        // the assertion while 5 more generators cross the promotion
        // threshold (3 resumes each).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function* g() { yield 1; yield 2; return 3; }\n\
                     var out = '';\n\
                     var first = g(); var r;\n\
                     while (!(r = first.next()).done) { out += r.value + ','; }\n\
                     out += r.value;\n\
                     for (var j = 1; j < 6; j++) { var it = g(); var rr; while (!(rr = it.next()).done) { } }\n\
                     out;",
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
        // with no catch escapes and completes the generator. Cut 69: the
        // body is resumed twice (below the promotion threshold), so it runs
        // interpreted — the behavior is identical, and the compiled throw
        // path is exercised by the sweep's loop-containing generators.
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
        assert_eq!(compiled, 0, "{compiled} bodies");
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
        // under Cut 69 the one-shot `.then` callback stays interpreted too
        // (below the promotion threshold) — the async-generator machinery
        // runs correctly either way.
        assert_eq!(compiled, 0, "{compiled} bodies");
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
        // Cut 69: every body here is one-shot (the async method, the inner
        // async function, and the `.then` callbacks), so all stay
        // interpreted — the capture-through-[[Environment]] behavior is
        // verified either way.
        assert_eq!(compiled, 0, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_array_destructure_declaration() {
        // Cut 59: a certified body's `let [a, b] = ...` compiles the
        // primitive `Destructure*` steps — the iterator opens via
        // `destructure_begin`, each element via `destructure_next` (landing
        // on the working stack, bound to the frame slots), the close via
        // `destructure_close`. Cut 69: `f` is straight-line, so the call
        // repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { let [a, b] = [1, 2]; return a + b * 10; }\n\
                     f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); \
                     f(); f(); f(); f(); f();",
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
                     f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); \
                     f(); f(); f(); f(); f(); f();",
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
                     f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); \
                     f(); f(); f(); f(); f(); f();",
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
                     f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); \
                     f(); f(); f(); f(); f(); f();",
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
        // (the `destructure_stepping` gate). Cut 69: the scenario lives in
        // `run` (straight-line), which repeats 17× — the script resets the
        // state per run, so the completion is the last run's output.
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
                     function run() {\n\
                     \x20 closed = 0; next_calls = 0;\n\
                     \x20 var err = '';\n\
                     \x20 try { let [a, b, c] = it(); } catch (e) { err = String(e); }\n\
                     \x20 return err + '|' + closed + '|' + next_calls;\n\
                     }\n\
                     run(); run(); run(); run(); run(); run(); run(); run(); run(); \
                     run(); run(); run(); run(); run(); run(); run(); run();",
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
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_destructure_closes_iterator_on_completion() {
        // Cut 59: a pattern that consumes FEWER values than the iterator
        // holds closes it on `DestructureClose` (spec 13.15.5.2 step 5) —
        // the iterator's `return` method runs. Cut 69: the scenario lives
        // in `run` (straight-line), which repeats 17× — `closed` is reset
        // per run, so the completion is the last run's count.
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
                     function run() { closed = 0; let [a] = it(); return closed; }\n\
                     run(); run(); run(); run(); run(); run(); run(); run(); run(); \
                     run(); run(); run(); run(); run(); run(); run(); run();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1.0));
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_destructure_captured_names() {
        // Cut 59: a destructured binding captured by a closure — the names
        // allocate capture-context slots and the pattern's `InitContextSlot`
        // binds them. Cut 69: `f` is straight-line, so the call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() {\n\
                     \x20 let [a, b] = [3, 4];\n\
                     \x20 return (function () { return a * 10 + b; })();\n\
                     }\n\
                     f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); \
                     f(); f(); f(); f(); f(); f();",
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
        // matches. Cut 69: the one-shot IIFE repeats 17× via the script
        // loop so it crosses the promotion threshold.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "'use strict';\n\
                     var make = function () { return arguments; };\n\
                     var r = make(1, true, 'a');\n\
                     var i = 0;\n\
                     while (i++ < 16) { make(1, true, 'a'); }\n\
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
                     var i = 0;\n\
                     while (i++ < 16) { g(1, true, 'a'); }\n\
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
        // count). Cut 69: `f` is straight-line, so the call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(a, b) { return a + '|' + arguments.length + '|' + arguments[0] + arguments[1]; }\n\
                     f(1, 2); f(1, 2); f(1, 2); f(1, 2); f(1, 2); f(1, 2); f(1, 2); f(1, 2); \
                     f(1, 2); f(1, 2); f(1, 2); f(1, 2); f(1, 2); f(1, 2); f(1, 2); f(1, 2); f(1, 2);",
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
        // is seen by the param and vice versa. Cut 69: both bodies are
        // straight-line, so the expression repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(a) { arguments[0] = 5; return a; }\n\
                     function g(a) { a = 7; return arguments[0]; }\n\
                     f(1) + '|' + g(1); f(1) + '|' + g(1); f(1) + '|' + g(1); \
                     f(1) + '|' + g(1); f(1) + '|' + g(1); f(1) + '|' + g(1); \
                     f(1) + '|' + g(1); f(1) + '|' + g(1); f(1) + '|' + g(1); \
                     f(1) + '|' + g(1); f(1) + '|' + g(1); f(1) + '|' + g(1); \
                     f(1) + '|' + g(1); f(1) + '|' + g(1); f(1) + '|' + g(1); \
                     f(1) + '|' + g(1); f(1) + '|' + g(1);",
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
        // not reflected). Cut 69: both bodies are straight-line, so the
        // expression repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { return typeof arguments.callee; }\n\
                     function g(a) { 'use strict'; a = 9; return arguments[0]; }\n\
                     f() + '|' + g(1); f() + '|' + g(1); f() + '|' + g(1); \
                     f() + '|' + g(1); f() + '|' + g(1); f() + '|' + g(1); \
                     f() + '|' + g(1); f() + '|' + g(1); f() + '|' + g(1); \
                     f() + '|' + g(1); f() + '|' + g(1); f() + '|' + g(1); \
                     f() + '|' + g(1); f() + '|' + g(1); f() + '|' + g(1); \
                     f() + '|' + g(1); f() + '|' + g(1);",
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
                     f(1); f(1); f(1); f(1); f(1); f(1); f(1); f(1); f(1); f(1); \
                     f(1); f(1); f(1); f(1); f(1); f(1); f(1);",
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
    fn typeof_ident_lowers() {
        // `TypeofIdent` pushes the `typeof` string with no operand: the test
        // double answers `"undefined"`, which is also what spec 13.5.3.2 step 1
        // requires for an unresolvable reference (the reason this is not
        // `LoadIdent` + `TypeofTop`). The step takes the name as an immediate.
        let engine = JitEngine::new().expect("native isa");
        let body = make_body(vec![Step::TypeofIdent { name: 1 }, Step::Return], 0);
        let compiled = engine.compile(&body, &helpers_all()).expect("lowers");
        let value = Value::from_bits(run(&compiled, 0));
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("undefined".to_string()),
            "the helper's typeof string is what the step pushes"
        );
        // Without the helper the body bails rather than resolving the name
        // itself (a compiled body has no scope to resolve against).
        assert!(engine.compile(&body, &helpers_none()).is_none());
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
    fn installed_jit_typeof_ident_matches_the_interpreter() {
        // The two `BindingLoc::Env` forms, both verified to emit the step (a
        // `typeof x` over a *captured* binding lowers to
        // `LoadContextSlot` + `TypeofTop` instead, which is why the closure
        // shape is not here): a name no record binds, and a global read from
        // inside a function body, which resolves through the environment chain
        // at run time. The first is the spec-critical one — an unresolvable
        // reference is "undefined", never a ReferenceError — and the second is
        // the resolvable branch of the helper.
        let source = "function free(n) { var s = 0; for (var i = 0; i < n; i++) { if (typeof no_such_name === 'undefined') s += 1; } return s; }\n\
                      function glob(n) { var s = ''; for (var i = 0; i < n; i++) { s = typeof Math; } return s; }\n\
                      free(4) + ':' + glob(4);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("jit runs"));
        assert_eq!(
            value, interp,
            "the typeof lowering must match the interpreter"
        );
        assert_eq!(
            value.as_string().map(|s| s.to_string()),
            Some("4:object".to_string())
        );
        assert_eq!(compiled, 2, "both loop bodies must compile");
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
    fn installed_jit_member_store_to_a_global_refreshes_the_read_cell() {
        // The compiled member-store fast path writes in place WITHOUT bumping
        // the receiver's generation, and a global object is additionally read
        // through the name-keyed global-value cell, which validates by that
        // generation — so the store has to refresh that cell, or the read below
        // serves the pre-write value. (`eval` bodies stay interpreted, so this
        // is the interpreter's `LoadIdent` probing a cell a compiled store
        // wrote; the same shape reaches a compiled reader — it is what
        // `language/types/reference/get-value-prop-base-primitive-realm.js`
        // caught across realms.)
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "globalThis.value = 1; \
                     function read() { return eval('value'); } \
                     function write(v) { for (var i = 0; i < 4; i++) { globalThis.value = v; } } \
                     function run() { var a = read(); write(''); var check = globalThis.value; \
                       var b = read(); \
                       return (a === 1 ? 100 : 0) + (check === '' ? 10 : 0) \
                              + (b === '' ? 1 : 0); } \
                     run();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_number(),
            Some(111.0),
            "100 = pre-write read wrong, 10 = the write did not land, 1 = post-write read stale"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_nested_ident_read_is_not_served_from_a_shadowing_chain() {
        // A DYNAMICALLY bound name in the chain is the hazard the per-name check
        // exists for: `eval("var x = 5")` puts `x` into `outer`'s own env, which
        // `inner`'s compile cannot see — so `inner` reads `x` as a `LoadIdent`,
        // and serving it from one of the shared-by-name cells would return the
        // GLOBAL 1 (warmed by `top`) instead of the injected 5. A gate that only
        // checked the chain's shape (no `with`, reaches the global record) sees
        // nothing wrong here.
        //
        // `run()` sequences the calls through LOCAL slots: a top-level
        // `var b = top()` would write a global object slot and bump the
        // generation, invalidating the cell the hazard needs to still be warm.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var x = 1; \
                     function top() { var s = 0; for (var i = 0; i < 2; i++) { s += x; } return s; } \
                     function outer() { eval('var x = 5'); \
                       function inner() { var s = 0; for (var i = 0; i < 2; i++) { s += x; } return s; } \
                       return inner(); } \
                     function run() { var b = top(); var c = outer(); \
                       return (b === 2 && c === 10) ? 1 : 0; } \
                     run();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_number(),
            Some(1.0),
            "the eval-injected binding wins over the warmed global cell"
        );
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_enclosing_declarations_are_captures_not_global_reads() {
        // The static shape of the hazard above is not one: a name the wrapper
        // DECLARES is compiled as a capture (a context slot) from the nested
        // body, so it never becomes a `LoadIdent` and never consults the cell.
        // Pinned so that if that ever changes, the cells cannot silently serve
        // the global value instead.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var x = 1; \
                     function top() { var s = 0; for (var i = 0; i < 2; i++) { s += x; } return s; } \
                     function outer() { const x = 2; \
                       function inner() { var s = 0; for (var i = 0; i < 2; i++) { s += x; } return s; } \
                       return inner(); } \
                     function run() { var a = outer(); var b = top(); var c = outer(); \
                       return (a === 4 && b === 2 && c === 4) ? 1 : 0; } \
                     run();",
                )
                .expect("runs")
        });
        assert_eq!(
            value.as_number(),
            Some(1.0),
            "the wrapper's own binding wins over the warmed global cell"
        );
        assert!(compiled >= 2, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_nested_global_read_sees_a_callees_write() {
        // A nested helper served from the cell must still observe a write to
        // the global: the generation bump invalidates the cell mid-loop.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var g = 1; \
                     function bump() { g = 2; } \
                     function outer() { \
                       function inner() { var s = 0; \
                         for (var i = 0; i < 3; i++) { if (i === 1) { bump(); } s += g; } \
                         return s; } \
                       return inner(); } \
                     outer() === 5 ? 1 : 0;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1.0), "the callee's write is seen");
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_declarative_binding_invalidates_a_warmed_object_cell() {
        // `K` is first an OBJECT-record global (a plain property of the global
        // object), which warms the global-value cell; a LATER script declares
        // `const K`, which changes what `K` resolves to while leaving the global
        // object untouched. The global env's generation bump is the only thing
        // that can invalidate the warmed cell — without it the compiled read
        // keeps serving 5 (the second completion would be 10, not 6). Neither
        // the declaration script nor the last one may write a global of its
        // own: that would bump the generation too and mask the hole.
        let (value, compiled) = with_jit_agent(|agent| {
            let first = agent
                .run_script(
                    "globalThis.K = 5; \
                     function rk() { var s = 0; for (var i = 0; i < 2; i++) { s += K; } return s; } \
                     rk();",
                )
                .expect("runs");
            assert_eq!(first.as_number(), Some(10.0), "the object-record read");
            agent.run_script("const K = 3;").expect("declares");
            agent.run_script("rk();").expect("runs")
        });
        assert_eq!(
            value.as_number(),
            Some(6.0),
            "the later declaration invalidated the warmed cell"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_declarative_read_sees_a_callees_write() {
        // The cell caches a top-level `let`'s value, so a write from a called
        // function must invalidate it: the loop reads 1, then 2, 2.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "let n = 1; \
                     function bump() { n = 2; } \
                     function rd() { var s = 0; \
                       for (var i = 0; i < 3; i++) { if (i === 1) { bump(); } s += n; } \
                       return s; } \
                     (rd() === 5) ? 1 : 0;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(1.0), "the callee's write is seen");
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_ident_read_with_scope_shadow_is_respected() {
        // A certified closure created inside a `with` reads a name the with
        // object shadows: its env chain contains a `with` scope, so the
        // compiled `LoadIdent` probe must be gated off (the cell for `x`
        // holds the global property's 1, not the with object's 2). The
        // per-call `globals_unshadowed` flag makes the probe miss to
        // `load_ident`, which resolves through the with env.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "var x = 1; with ({ x: 2 }) { var f = function () { return x; }; } \
                     f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); \
                     f(); f(); f(); f(); f();",
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
                     function f() { return x; } f(); f(); f(); f(); f(); f(); f(); f(); f(); f(); \
                     f(); f(); f(); f(); f(); f(); f();",
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
        // `o.x += 1` through the real runtime machinery. Cut 69: `f` is
        // straight-line, so the call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(o) { o.x += 1; return o.x; } \
                     f({ x: 41 }); f({ x: 41 }); f({ x: 41 }); f({ x: 41 }); f({ x: 41 }); \
                     f({ x: 41 }); f({ x: 41 }); f({ x: 41 }); f({ x: 41 }); f({ x: 41 }); \
                     f({ x: 41 }); f({ x: 41 }); f({ x: 41 }); f({ x: 41 }); f({ x: 41 }); \
                     f({ x: 41 }); f({ x: 41 });",
                )
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
        // helpers would resolve the caller's env. Cut 69: `f` is
        // straight-line, so the call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function make() { var x = 41; return function f() { return x + 1; }; }\n\
                     var f = make(); f(); f(); f(); f(); f(); f(); f(); f(); f(); \
                     f(); f(); f(); f(); f(); f(); f(); f();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(42.0));
        assert!(compiled >= 1, "{compiled} bodies");

        // A captured write (`x = x + 41` reads then stores) and the fused
        // update (`++x` reads, updates, stores, returns the new value). Cut
        // 69: `f` mutates its captured `x`, so the call must NOT repeat on
        // one closure — each `make()()` is a fresh closure with a fresh `x`
        // (the inner function's body is one shared site, so 17 calls cross
        // the promotion threshold; each returns 43).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function make() { var x = 1; return function f() { x = x + 41; return ++x; }; }\n\
                     make()(); make()(); make()(); make()(); make()(); make()(); \
                     make()(); make()(); make()(); make()(); make()(); make()(); \
                     make()(); make()(); make()(); make()(); make()();",
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
        // through the per-iteration env machinery). Cut 69: the captured
        // closure is straight-line, so `make()()` repeats 17× (the
        // completion stays the last call's value).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function make() { var fns = []; for (let i = 0; i < 3; i++) { fns.push(function () { return i * 10; }); } return fns[2]; }\n\
                     make()(); make()(); make()(); make()(); make()(); make()(); \
                     make()(); make()(); make()(); make()(); make()(); make()(); \
                     make()(); make()(); make()(); make()(); make()();",
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
                     make()(); make()(); make()(); make()(); make()(); make()(); \
                     make()(); make()(); make()(); make()(); make()(); make()(); \
                     make()(); make()(); make()(); make()(); make()();",
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
                .run_script(
                    "function g(o) { var v = o.x; o.x = 42; return v; } \
                     g({ x: 41 }); g({ x: 41 }); g({ x: 41 }); g({ x: 41 }); g({ x: 41 }); \
                     g({ x: 41 }); g({ x: 41 }); g({ x: 41 }); g({ x: 41 }); g({ x: 41 }); \
                     g({ x: 41 }); g({ x: 41 }); g({ x: 41 }); g({ x: 41 }); g({ x: 41 }); \
                     g({ x: 41 }); g({ x: 41 });",
                )
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
            // The top-level bench shapes. The straight-line cases below run
            // interpreted under Cut 69 (a consulted-once script body is
            // below the promotion threshold), so their `min_compiled` is 0;
            // the compiled completion register is exercised by the loop
            // cases.
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
            ("var x = 1;", None, 0),
            // A statement-position assignment carries its value
            // (`FusedStoreLocal` sets the completion).
            ("var x; x = 5;", Some(5.0), 0),
            // Control statements: with and without a value.
            ("if (true) { 3 }", Some(3.0), 0),
            ("if (false) { 3 }", None, 0),
            // A trailing block that ends empty restores the pre-block
            // completion (`5; { var q = 1; }` completes 5, not undefined).
            ("5; { var q = 1; }", Some(5.0), 0),
            // ...but a control statement inside the block (ResetCompletion +
            // NormalizeCompletion) turns the register empty, so the block's
            // empty end does not restore 5.
            ("5; { if (true) {} }", None, 0),
            // The fused call-store only fires for plain slot args; a literal
            // arg keeps the `FusedStoreLocal` tail, which sets the completion.
            (
                "function g(x) { return x + 1; } var s = 0; s = g(1);",
                Some(2.0),
                0,
            ),
            // In the counter path the loop body's `FusedStoreLocal` sets the
            // completion on the last iteration (the last `s = g(i)` = 3).
            // `g` is called 3 times — below the promotion threshold — so
            // only the script body compiles.
            (
                "function g(x) { return x + 1; } var s = 0; for (var i = 0; i < 3; i++) { s = g(i); }",
                Some(3.0),
                1,
            ),
            // A segmented loop body (register run + step-path `if`) inside
            // a block: the loop body's `ListBegin`/`ListEnd` stay on the
            // step path (the register run must not absorb one), and the
            // completion matches the non-segmented model (a control
            // statement normalizes its empty completion to undefined, so
            // the block's ListEnd does not restore the pre-block value).
            (
                "var a = []; var l = 0; 1; { for (var i = 0; i < 2; i++) { a[l++] = i; } }",
                None,
                1,
            ),
            (
                "var a = []; var l = 0; { 1; { for (var i = 0; i < 2; i++) { a[l++] = i; } } }",
                None,
                1,
            ),
            (
                "var a = []; var l = 0; { for (var i = 0; i < 2; i++) { a[l++] = i; } }",
                None,
                1,
            ),
            (
                "var a = []; var l = 0; 1; \
                 { for (var i = 0; i < 2; i++) { a[l++] = i; if (i === 1) { } } }",
                None,
                1,
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
        // the constructed object. Cut 69: `C` is straight-line, so the
        // construct repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function C(x) { this.v = x; } \
                     new C(5).v; new C(5).v; new C(5).v; new C(5).v; new C(5).v; \
                     new C(5).v; new C(5).v; new C(5).v; new C(5).v; new C(5).v; \
                     new C(5).v; new C(5).v; new C(5).v; new C(5).v; new C(5).v; \
                     new C(5).v; new C(5).v;",
                )
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
    fn installed_jit_dense_array_element_write_fast_path() {
        // The inline dense-array store: `a[a.length] = i` in a compiled
        // loop must take the Phase C machine-code append — the counting
        // wrapper proves the helper runs only for growth/fallback, not per
        // element — and store every element. The fallback shapes — a
        // non-Array receiver or a non-canonical-index key — still land
        // through `fast_array_element_write`/`assign_member_computed`.
        static APPEND_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_append(
            ctx: *mut c_void,
            object: u64,
            index: u64,
            value: u64,
        ) -> u64 {
            APPEND_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.dense_array_append)(ctx, object, index, value)
        }
        let mut helpers = runtime_helpers();
        helpers.dense_array_append = Some(counting_append);

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
                "function fill(a, n) { for (var i = 0; i < n; i++) { a[a.length] = i; } return a.length; }\n\
                 var a = [];\n\
                 var len = fill(a, 1000);\n\
                 var fallback = {};\n\
                 fallback['x'] = 5;\n\
                 var arr2 = [];\n\
                 arr2['y'] = 7;\n\
                 a[0] + a[999] + len + fallback.x + arr2.y;",
            )
            .expect("runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        // 0 + 999 + 1000 + 5 + 7 = 2011.
        assert_eq!(value.as_number(), Some(2011.0));
        assert!(compiled >= 1, "{compiled} bodies");
        let calls = APPEND_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            calls < 100,
            "the compiled fill loop must inline the append (the helper runs only on growth; {calls} calls)"
        );
    }

    #[test]
    fn installed_jit_dense_array_element_read_inlines() {
        // The inline dense-array element read: `a[i]` over a dense Array in a
        // compiled loop must take the machine-code buffer read — the counting
        // wrapper proves the `get_member_computed` helper runs only for the
        // declined shapes, not per element — and the value must match the
        // interpreter.
        static READ_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_read(ctx: *mut c_void, object: u64, key: u64) -> u64 {
            READ_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.get_member_computed)(ctx, object, key)
        }
        let mut helpers = runtime_helpers();
        helpers.get_member_computed = Some(counting_read);

        // The loop is the inlined shape (a dense, in-bounds, non-hole element);
        // the tail exercises the declines — an out-of-range index, a string
        // key, and a non-Array receiver — which must still go through the
        // helper.
        let source = "function sum(a, n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) { s += a[i & 255]; }\n\
                        return s;\n\
                      }\n\
                      var a = new Array(256);\n\
                      for (var i = 0; i < 256; i++) { a[i] = i; }\n\
                      var r = sum(a, 100000);\n\
                      var oob = a[99999];\n\
                      var sk = a['x'];\n\
                      var obj = { 0: 42 };\n\
                      var nr = obj[0];\n\
                      r + (oob === undefined ? 1 : 0) + (sk === undefined ? 1 : 0) + nr;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value, interp, "the dense read must match the interpreter");
        assert!(compiled >= 1, "{compiled} bodies must compile");
        let reads = READ_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            reads < 100,
            "the compiled read loop must inline the dense element read ({reads} get_member_computed calls)"
        );
    }

    #[test]
    fn installed_jit_computed_read_cell_inlines() {
        // G8: a compiled `o[k]` read with a String key over an own data property
        // must serve from the computed-read cell — the counting wrapper proves
        // the `get_member_computed` helper runs only while the cell warms up, not
        // per read — and the value must match the interpreter. The object holds
        // ONE key deliberately: the probe's value comes from the 16-slot
        // `member_value_cells` (keyed `(id ^ atom) & 15`), so a many-key object
        // can alias there and the count would then depend on which atoms the
        // process-global interner happened to assign (the parallel suite shifts
        // that); the multi-key shape is checked differentially below. The loop
        // writes through the same key so the read cannot be CSE'd away.
        static READ_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_read(ctx: *mut c_void, object: u64, key: u64) -> u64 {
            READ_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.get_member_computed)(ctx, object, key)
        }
        let mut helpers = runtime_helpers();
        helpers.get_member_computed = Some(counting_read);

        // The loop is the cell-served shape (an own data property, one String
        // key); the tail exercises the declines — a Number key, an unrecorded
        // String key, and a primitive receiver — which must still go through the
        // helper.
        let source = "function sum(o, k, n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) { o[k] = i; s += o[k]; }\n\
                        return s;\n\
                      }\n\
                      var o = { a: 0 };\n\
                      var r = sum(o, 'a', 100000);\n\
                      var num = o[0];\n\
                      var miss = o['zzz'];\n\
                      var prim = (5)['x'];\n\
                      r + (num === undefined ? 1 : 0) + (miss === undefined ? 1 : 0)\n\
                        + (prim === undefined ? 1 : 0);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(
            value, interp,
            "the computed read must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies must compile");
        let reads = READ_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            reads < 100,
            "the compiled read loop must serve from the computed-read cell ({reads} get_member_computed calls)"
        );
    }

    #[test]
    fn installed_jit_computed_read_multi_key_matches_the_interpreter() {
        // The five-key shape (the `str_key` probe): its per-read hit rate depends
        // on the 16-slot `member_value_cells` layout (see the inline test above),
        // but the VALUE must match the interpreter for every key.
        let source = "function sum(o, keys, n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) { s += o[keys[i % 5]]; }\n\
                        return s;\n\
                      }\n\
                      var o = { a: 1, b: 2, c: 3, d: 4, e: 5 };\n\
                      var keys = ['a', 'b', 'c', 'd', 'e'];\n\
                      sum(o, keys, 100000);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("runs"));
        assert_eq!(
            value, interp,
            "the multi-key computed read must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_computed_read_after_inplace_store_matches_the_interpreter() {
        // G8: an in-place value write (`o[k] = v`, the compiled `SetMemberSlot`
        // and the interpreter's warm store) deliberately does NOT bump the
        // receiver's generation — it refreshes the read-side value cell instead.
        // The compiled computed read takes its VALUE from that cell, so a store
        // in the loop must be visible. A cell that cached the value itself would
        // serve a stale read here (the corpus caught exactly this).
        let source = "function bump(o, keys, n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) {\n\
                          var k = keys[i % 5];\n\
                          s += o[k];\n\
                          o[k] = o[k] + 1;\n\
                        }\n\
                        return s;\n\
                      }\n\
                      var o = { a: 0, b: 0, c: 0, d: 0, e: 0 };\n\
                      var keys = ['a', 'b', 'c', 'd', 'e'];\n\
                      var r = bump(o, keys, 1000);\n\
                      r + o.a + o.b + o.c + o.d + o.e;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("runs"));
        assert_eq!(
            value, interp,
            "a store must invalidate the compiled computed read"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_direct_break_skips_the_control_helper() {
        // G18: a `break` whose transfer has no finally to route through and no
        // for-of iterator to close is exactly `ip = target`, so the compiled
        // code jumps directly — no `BreakControl` helper and no epoch bump. The
        // counting wrapper isolates the helper; the switch body (no try, no
        // for-of) is the eligible shape, and the value must match the
        // interpreter.
        static BREAK_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_break(ctx: *mut c_void, ip: u64, target: u64) -> u64 {
            BREAK_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.break_control)(ctx, ip, target)
        }
        let mut helpers = runtime_helpers();
        helpers.break_control = Some(counting_break);
        let source = "function sum(n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) {\n\
                          switch (i & 3) {\n\
                            case 0: s += 1; break;\n\
                            case 1: s += 2; break;\n\
                            case 2: s += 3; break;\n\
                            default: s += 4; break;\n\
                          }\n\
                        }\n\
                        return s;\n\
                      }\n\
                      sum(100000);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value, interp, "the switch break must match the interpreter");
        assert!(compiled >= 1, "{compiled} bodies must compile");
        let calls = BREAK_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            calls < 100,
            "the compiled switch break must jump directly ({calls} break_control calls)"
        );
    }

    #[test]
    fn installed_jit_break_through_a_finally_still_uses_the_control_helper() {
        // The G18 gate is exactly "no try and no for-of in the body", so a
        // `break` that leaves a try/finally must still transfer through the
        // helper (the finally runs before the loop exits). The counting wrapper
        // must see calls, and the value must match the interpreter.
        static BREAK_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_break(ctx: *mut c_void, ip: u64, target: u64) -> u64 {
            BREAK_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.break_control)(ctx, ip, target)
        }
        let mut helpers = runtime_helpers();
        helpers.break_control = Some(counting_break);
        let source = "function f(n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) {\n\
                          try { if (i & 1) { break; } s += 1; } finally { s += 10; }\n\
                        }\n\
                        return s;\n\
                      }\n\
                      f(10);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(
            value, interp,
            "the break through a finally must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies must compile");
        let calls = BREAK_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            calls > 0,
            "a break leaving a finally must route through the helper"
        );
        assert!(
            calls > 0,
            "a break leaving a finally must route through the helper"
        );
    }

    #[test]
    fn installed_jit_switch_disc_and_test_inline() {
        // G19: a `switch` over Numbers resolves its discriminant and its case
        // tests in machine code — one Vm store for the discriminant and one f64
        // compare per case — so neither `SwitchDisc` nor `SwitchTest` runs the
        // helper. The counting wrappers isolate both; the value must match the
        // interpreter.
        static DISC_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static TEST_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_disc(ctx: *mut c_void, value: u64) -> u64 {
            DISC_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.switch_disc)(ctx, value)
        }
        extern "C" fn counting_test(ctx: *mut c_void, case: u64, test: u64) -> u64 {
            TEST_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.switch_test)(ctx, case, test)
        }
        let mut helpers = runtime_helpers();
        helpers.switch_disc = Some(counting_disc);
        helpers.switch_test = Some(counting_test);
        let source = "function sum(n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) {\n\
                          switch (i & 3) {\n\
                            case 0: s += 1; break;\n\
                            case 1: s += 2; break;\n\
                            case 2: s += 3; break;\n\
                            default: s += 4; break;\n\
                          }\n\
                        }\n\
                        return s;\n\
                      }\n\
                      sum(100000);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value, interp, "the switch must match the interpreter");
        assert!(compiled >= 1, "{compiled} bodies must compile");
        let disc = DISC_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        let test = TEST_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            disc < 100,
            "the compiled discriminant must not call the helper ({disc} switch_disc calls)"
        );
        assert!(
            test < 100,
            "the compiled case tests must not call the helper ({test} switch_test calls)"
        );
    }

    #[test]
    fn installed_jit_switch_mixed_case_falls_to_the_helper() {
        // The G19 fast paths decline a case test whose kind differs from the
        // discriminant (a String case against a Number discriminant is never
        // `===`), so that test resolves through the `switch_test` helper. The
        // value must match the interpreter.
        static TEST_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_test(ctx: *mut c_void, case: u64, test: u64) -> u64 {
            TEST_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.switch_test)(ctx, case, test)
        }
        let mut helpers = runtime_helpers();
        helpers.switch_test = Some(counting_test);
        let source = "function f(n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) {\n\
                          switch (i % 4) {\n\
                            case 1: s += 1; break;\n\
                            case 2: s += 2; break;\n\
                            default: s += 3; break;\n\
                            case '1': s += 100; break;\n\
                          }\n\
                        }\n\
                        return s;\n\
                      }\n\
                      f(10000);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(
            value, interp,
            "the mixed-case switch must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies must compile");
        let calls = TEST_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            calls > 0,
            "a kind-mismatched case must resolve through the helper"
        );
    }

    #[test]
    fn installed_jit_switch_edge_values_match_the_interpreter() {
        // The inline strict-equality's three paths, differentially: `0`/`-0`
        // (the f64 compare — equal), `true`/`null`/`undefined` (identical bits),
        // a String case and `1` against a String discriminant (the helper), and
        // `NaN` (a Number, never `===` anything, so it always reaches default).
        let source = "function f(vals, n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) {\n\
                          switch (vals[i % 8]) {\n\
                            case 0: s += 1; break;\n\
                            case -0: s += 2; break;\n\
                            case true: s += 3; break;\n\
                            case null: s += 4; break;\n\
                            case undefined: s += 5; break;\n\
                            case 'x': s += 6; break;\n\
                            case 1: s += 7; break;\n\
                            default: s += 8; break;\n\
                          }\n\
                        }\n\
                        return s;\n\
                      }\n\
                      var vals = [0, -0, true, null, undefined, 'x', 1, NaN];\n\
                      f(vals, 8000);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("runs"));
        assert_eq!(
            value, interp,
            "the switch edge values must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_dense_element_overwrite_inlines() {
        // G20: `a[k] = v` with a canonical index BELOW the length (an in-place
        // overwrite, not an append) must write in machine code — the counting
        // wrappers prove neither the register path's `SetMemberComputed` nor the
        // step path's `FastArrayElementWrite` runs per store — and the value must
        // match the interpreter. The `[]`-then-append fill keeps the
        // materialization off the counters (the append arm is already inline).
        static SET_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static WRITE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_set(ctx: *mut c_void, object: u64, key: u64, value: u64) -> u64 {
            SET_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.set_member_computed)(ctx, object, key, value)
        }
        extern "C" fn counting_write(ctx: *mut c_void, object: u64, key: u64, value: u64) -> u64 {
            WRITE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.fast_array_element_write)(ctx, object, key, value)
        }
        let mut helpers = runtime_helpers();
        helpers.set_member_computed = Some(counting_set);
        helpers.fast_array_element_write = Some(counting_write);
        let source = "function fill(a, n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) { var k = i & 1023; a[k] = i; s += a[k]; }\n\
                        return s;\n\
                      }\n\
                      var a = [];\n\
                      for (var i = 0; i < 1024; i++) { a[i] = i; }\n\
                      fill(a, 100000);";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(value, interp, "the overwrite must match the interpreter");
        assert!(compiled >= 1, "{compiled} bodies must compile");
        let set = SET_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        let write = WRITE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            set < 100,
            "the in-place dense store must inline ({set} set_member_computed calls)"
        );
        assert!(
            write < 100,
            "the in-place dense store must inline ({write} fast_array_element_write calls)"
        );
    }

    #[test]
    fn installed_jit_dense_element_overwrite_declines_match_the_interpreter() {
        // The G20 gates' declines, differentially: a grow / hole-fill (an index
        // at or above the length), a registered prototype (the generation
        // bump's elements-protector side effect), and heap object values (the
        // write barrier).
        let source = "function f(n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) {\n\
                          var k = i % 6;\n\
                          a[k] = i;\n\
                          s += (a[k] === undefined ? 0 : a[k]);\n\
                          p[i % 3] = i;\n\
                          h[i % 3] = { v: i };\n\
                        }\n\
                        return s;\n\
                      }\n\
                      var a = [1, 2, 3];\n\
                      var p = [1, 2, 3];\n\
                      var child = Object.create(p);\n\
                      var h = [null, null, null];\n\
                      f(1000);\n\
                      a.length + a[0] + a[5] + child[0] + p[2] + h[0].v + h[2].v;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("runs"));
        assert_eq!(
            value, interp,
            "the overwrite declines must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_dense_element_read_declines_holes_to_the_chain() {
        // A dense Array element read is exact only for a real own value: a HOLE
        // is spec-absent, so the read must fall to the helper and consult the
        // prototype chain. The compiled loop reads holes every iteration, and
        // the prototype elements must win — matching the interpreter. A hole
        // served as a value (the sentinel bits) would diverge here.
        let source = "function read(a, n) {\n\
                        var s = 0;\n\
                        for (var i = 0; i < n; i++) { s += a[i & 3]; }\n\
                        return s;\n\
                      }\n\
                      var a = [];\n\
                      a[2] = 1;\n\
                      Array.prototype[0] = 10;\n\
                      Array.prototype[1] = 20;\n\
                      Array.prototype[3] = 40;\n\
                      var r = read(a, 1000);\n\
                      delete Array.prototype[0];\n\
                      delete Array.prototype[1];\n\
                      delete Array.prototype[3];\n\
                      r;";
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("runs"));
        assert_eq!(value, interp, "the hole read must match the interpreter");
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_typed_array_element_read_inlines() {
        // The inline TypedArray element read: `ta[k]` over a numeric element
        // kind in a compiled loop must take the machine-code load — the
        // counting wrapper proves the `get_member_computed` helper runs only
        // for the declined shapes, not per element — and the values must match
        // the interpreter.
        static READ_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_read(ctx: *mut c_void, object: u64, key: u64) -> u64 {
            READ_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.get_member_computed)(ctx, object, key)
        }
        let mut helpers = runtime_helpers();
        helpers.get_member_computed = Some(counting_read);
        let source = r#"function sum(a, n) {
                        var s = 0;
                        for (var i = 0; i < n; i++) { s += a[i & 63]; }
                        return s;
                      }
                      var u8 = new Uint8Array(64);
                      var i32 = new Int32Array(64);
                      var f64 = new Float64Array(64);
                      for (var i = 0; i < 64; i++) {
                        u8[i] = i; i32[i] = i * 1000; f64[i] = i / 4;
                      }
                      sum(u8, 10000) + sum(i32, 10000) + sum(f64, 10000);"#;
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };

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
        let value = agent.run_script(source).expect("jit runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        assert_eq!(
            value, interp,
            "the typed-array read must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies must compile");
        let reads = READ_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        // The `workers` build cfg's the shared-buffer block layout, so the
        // typed-array lane deliberately declines to the helper there (as the
        // inline store does) and every read counts. The inline assertion is
        // only meaningful in the single-agent build (`cargo test -p jit`).
        if !crux::typed_array::WORKERS {
            assert!(
                reads < 50,
                "the compiled read loop must inline the typed-array element read ({reads} get_member_computed calls)"
            );
        }
    }

    #[test]
    fn installed_jit_typed_array_element_read_matches_the_interpreter() {
        // Every supported element kind through a compiled read loop, plus the
        // declined shapes: an out-of-range / non-canonical key, a non-typed-
        // array receiver, a detached buffer, a resizable-buffer view, and the
        // BigInt element kinds (a helper allocation). The comparison is the
        // exactness check.
        let source = r#"function read(a, n) {
                        var s = 0;
                        for (var i = 0; i < n; i++) { s += a[i & 7]; }
                        return s;
                      }
                      var kinds = [
                        new Int8Array([-1, 2, -3, 4, -5, 6, -7, 8]),
                        new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]),
                        new Uint8ClampedArray([1, 2, 3, 4, 5, 6, 7, 8]),
                        new Int16Array([-100, 200, -300, 400, -500, 600, -700, 800]),
                        new Uint16Array([1, 2, 3, 4, 5, 6, 7, 8]),
                        new Int32Array([-100000, 2, -3, 4, -5, 6, -7, 8]),
                        new Uint32Array([4000000000, 2, 3, 4, 5, 6, 7, 8]),
                        new Float32Array([1.5, -2.25, 3.125, 4, 5, 6, 7, 8]),
                        new Float64Array([1.5, -2.25, 3.125, 4.5, 5.5, 6.5, 7.5, 8.5])
                      ];
                      var total = 0;
                      for (var k = 0; k < kinds.length; k++) { total += read(kinds[k], 800); }
                      var small = new Uint8Array([9, 8, 7, 6]);
                      total += (small[99] === undefined ? 1 : 0);
                      total += (small['x'] === undefined ? 1 : 0);
                      total += (small[1.5] === undefined ? 1 : 0);
                      total += read({ 0: 7, 1: 7, 2: 7, 3: 7, 4: 7, 5: 7, 6: 7, 7: 7 }, 800);
                      if (typeof BigInt64Array === 'function') {
                        var big = new BigInt64Array(4);
                        big[1] = 5n;
                        total += (big[1] === 5n ? 1 : 0);
                        total += (big[3] === 0n ? 1 : 0);
                      }
                      if (typeof structuredClone === 'function') {
                        try {
                          var ab = new ArrayBuffer(8);
                          var dview = new Uint8Array(ab);
                          dview[0] = 7;
                          structuredClone(ab, { transfer: [ab] });
                          total += (dview[0] === undefined ? 1 : 0);
                        } catch (e) { total += 0; }
                      }
                      if (typeof ArrayBuffer.prototype.resize === 'function') {
                        var rab = new ArrayBuffer(8, { maxByteLength: 16 });
                        var rview = new Uint8Array(rab);
                        rview[0] = 3;
                        total += read(rview, 800);
                        rab.resize(4);
                        total += (rview[3] === undefined ? 1 : 0);
                      }
                      total;"#;
        let interp = {
            let mut agent = runtime::Agent::new();
            agent.initialize_host_defined_realm().expect("realm");
            agent.run_script(source).expect("interp runs")
        };
        let (value, compiled) = with_jit_agent(|agent| agent.run_script(source).expect("runs"));
        assert_eq!(
            value, interp,
            "every typed-array read kind and decline must match the interpreter"
        );
        assert!(compiled >= 1, "{compiled} bodies");
    }

    /// A6: the compiled safe point drives the nursery. The compiled loop's
    /// allocations are garbage, so a minor reclaims them and stays enabled; the
    /// nursery threshold is lowered so the minor level answers before the major's
    /// growth trigger (live > 2x the post-major live count) does, which is the
    /// routing `maybe_collect` performs for both engines.
    ///
    /// The cohort bound is the `JIT_GC_PROBE_INTERVAL` evidence: the machine code
    /// polls the budget every 1024 iterations, so a compiled loop's cohort can
    /// overshoot a threshold set below that interval but not run away — a cohort
    /// near the loop's total allocation count would mean the compiled safe point
    /// never reached the collector.
    #[test]
    fn installed_jit_compiled_loop_safe_point_runs_minors() {
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
        agent.set_nursery_threshold(512);
        crux::heap::set_gc_trace(true);
        let _ = crux::heap::take_gc_trace_records();
        let value = agent
            .run_script(
                "var a = [];\n\
                 var l = 0;\n\
                 for (var i = 0; i < 200000; i++) { var o = { x: i }; a[l++] = i; }\n\
                 a.length + a[0] + a[199999];",
            )
            .expect("runs");
        let records = crux::heap::take_gc_trace_records();
        crux::heap::set_gc_trace(false);
        let compiled = cache.compiled_count();
        agent.jit_hook = None;

        // 200000 + 0 + 199999.
        assert_eq!(value.as_number(), Some(399999.0));
        assert!(compiled >= 1, "{compiled} bodies");
        let minors: Vec<_> = records.iter().filter(|r| r.level == "minor").collect();
        assert!(
            !minors.is_empty(),
            "a compiled loop's safe point must run minors ({} collections)",
            records.len()
        );
        let worst = minors.iter().map(|r| r.young).max().unwrap_or(0);
        assert!(
            worst < 20000,
            "the compiled probe must pace the cohort (peaked at {worst} over 200k allocations)"
        );
    }

    /// A6: a primitive append into a PROMOTED dense array still takes the inline
    /// path. The write barrier is a no-op for a non-heap value, so the
    /// ArraySlots box's age is irrelevant for a Number store — a container-only
    /// young guard sent every append of an old array to the fallback (measured
    /// 11ms -> 17ms per 1M appends, against 11ms for a young array).
    ///
    /// The fallback is the caller's legacy tail (the computed-member store's
    /// `SetMemberComputed`), NOT the gate's `dense_array_append`: that helper
    /// serves the growth case, which stays inline-eligible either way. So the
    /// count of `set_member_computed` calls is the guard's signal — zero for
    /// canonical appends of primitives, one per iteration if the guard bails on
    /// the container's age.
    ///
    /// `collect_garbage` between the two scripts promotes the array (a major
    /// clears the young bit on every survivor), so the guard's old branch is
    /// what this pins.
    #[test]
    fn installed_jit_primitive_append_into_a_promoted_array_stays_inline() {
        static SLOW_STORE_CALLS: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_set_member_computed(
            ctx: *mut c_void,
            object: u64,
            key: u64,
            value: u64,
        ) -> u64 {
            SLOW_STORE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.set_member_computed)(ctx, object, key, value)
        }
        let mut helpers = runtime_helpers();
        helpers.set_member_computed = Some(counting_set_member_computed);

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
        agent.run_script("var a = [];").expect("runs");
        agent.collect_garbage();
        let value = agent
            .run_script(
                "function fill(a, n) { var l = a.length; for (var i = 0; i < n; i++) { a[l++] = i; } return a.length; }\n\
                 fill(a, 100000);",
            )
            .expect("runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;

        assert_eq!(value.as_number(), Some(100000.0));
        assert!(compiled >= 1, "{compiled} bodies");
        let calls = SLOW_STORE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            calls, 0,
            "primitive appends into a promoted array must stay inline \
             ({calls} fell through to the computed-member store)"
        );
    }

    #[test]
    fn installed_jit_register_store_member_computed_takes_the_inline_append() {
        // The Phase C register-path append: `a[l++] = i` lowers to a
        // register body (`StoreMemberComputed { key: PostInc(l), value:
        // Counter }`) whose compiled store must inline the append — the
        // counting wrapper proves the helper runs only on growth — and store
        // every element.
        static APPEND_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        extern "C" fn counting_append(
            ctx: *mut c_void,
            object: u64,
            index: u64,
            value: u64,
        ) -> u64 {
            APPEND_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (runtime::jit::JIT_SLOW_PATHS.dense_array_append)(ctx, object, index, value)
        }
        let mut helpers = runtime_helpers();
        helpers.dense_array_append = Some(counting_append);

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
                "function fill(n) { var a = []; var l = 0; for (var i = 0; i < n; i++) { a[l++] = i; } return a[0] + a[n - 1] + a.length; }\n\
                 fill(1000);",
            )
            .expect("runs");
        let compiled = cache.compiled_count();
        agent.jit_hook = None;
        // 0 + 999 + 1000 = 1999.
        assert_eq!(value.as_number(), Some(1999.0));
        assert!(compiled >= 1, "{compiled} bodies");
        let calls = APPEND_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            calls < 100,
            "the compiled register store must inline the append (the helper runs only on growth; {calls} calls)"
        );
    }

    #[test]
    fn installed_jit_dense_array_element_write_fallbacks() {
        // The inline store's fallback shapes must route through the full
        // `assign_member_computed` helper and land correctly: a non-Array
        // receiver, a non-canonical-index key (a string key on an Array),
        // and a compound computed assign (which never certifies — the
        // `GetMemberComputedKeep` step has no JIT arm — so its
        // `AssignMemberComputed { op }` with a compound stays on the helper
        // by the emit's `!is_compound_assign` guard).
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function fill(a, n) { for (var i = 0; i < n; i++) { a[a.length] = i; } return a; }\n\
                     var a = fill([], 5);\n\
                     var o = {};\n\
                     o[0] = 9;\n\
                     var b = [];\n\
                     b['k'] = 3;\n\
                     var c = [1];\n\
                     c[0] += 4;\n\
                     a[0] + a[4] + o[0] + b.k + c[0];",
                )
                .expect("runs")
        });
        // 0 + 4 + 9 + 3 + 5 = 21.
        assert_eq!(value.as_number(), Some(21.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_dense_array_element_write_fallback_in_a_compiled_loop() {
        // The inline store's FALLBACK (a non-Array receiver and a string-key
        // Array store) inside a COMPILED loop: the step must leave the same
        // single slot the fast path does. The pre-fix emit pushed the slow
        // helper's result AND the value, so every fallback iteration drifted
        // the working stack one slot deeper than `max_stack_usage` accounts —
        // the writes ran past the pre-allocated buffer and corrupted the heap
        // (the detached-buffer copyWithin cluster died with a 1-2 TB
        // `JsString::from_utf8` allocation in `dispatch_error`). The
        // top-level fallback test above misses this: straight-line scripts
        // never compile, so its fallbacks ran interpreted.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(n) {\n\
                       var o = {};\n\
                       var b = [];\n\
                       for (var i = 0; i < n; i++) { o[i] = i; b['x'] = i; }\n\
                       return o[n - 1] + b.x;\n\
                     }\n\
                     f(1000);",
                )
                .expect("runs")
        });
        // 999 + 999 = 1998.
        assert_eq!(value.as_number(), Some(1998.0));
        assert!(compiled >= 1, "{compiled} bodies");
    }

    #[test]
    fn installed_jit_non_canonical_post_inc_keys_fall_back_cleanly() {
        // A register computed store whose PostInc key is a real double that
        // is NOT a canonical array index must fall back to the slow helper
        // and land exactly like the interpreter. The dense-append gate used a
        // NON-saturating `fcvt_to_uint`, which the x64 backend lowers to a
        // trap on NaN, an infinity, a negative, or a value >= 2^63 — the
        // machine code died with an illegal instruction before its range
        // checks could route the key to the legacy path. Each function has a
        // loop, so it compiles on its first call and runs the machine store.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function fn_nan() { var a = []; var l = NaN; for (var i = 0; i < 2; i++) { a[l++] = i; } return (a.NaN === 1 && l !== l) ? 0 : 1; }\n\
                     function fn_inf() { var a = []; var l = Infinity; for (var i = 0; i < 1; i++) { a[l++] = 5; } return (a.Infinity === 5 && l === Infinity) ? 0 : 1; }\n\
                     function fn_ninf() { var a = []; var l = -Infinity; for (var i = 0; i < 1; i++) { a[l++] = 6; } return (a['-Infinity'] === 6 && l === -Infinity) ? 0 : 1; }\n\
                     function fn_neg() { var a = []; var l = -1; for (var i = 0; i < 1; i++) { a[l++] = 5; } return (a['-1'] === 5 && l === 0 && a.length === 0) ? 0 : 1; }\n\
                     function fn_frac() { var a = []; var l = 2.5; for (var i = 0; i < 1; i++) { a[l++] = 7; } return (a['2.5'] === 7 && l === 3.5) ? 0 : 1; }\n\
                     function fn_huge() { var a = []; var l = 9223372036854775808; for (var i = 0; i < 1; i++) { a[l++] = 2; } return (a['9223372036854776000'] === 2 && a.length === 0) ? 0 : 1; }\n\
                     function fn_bigint() { var a = []; var l = 1n; for (var i = 0; i < 1; i++) { a[l++] = 9; } return (a.length === 2 && a[1] === 9 && l === 2n) ? 0 : 1; }\n\
                     fn_nan() + fn_inf() + fn_ninf() + fn_neg() + fn_frac() + fn_huge() + fn_bigint();",
                )
                .expect("runs")
        });
        assert_eq!(value.as_number(), Some(0.0));
        // The loop bodies compile (5 of the 7 functions — the rest stay
        // interpreted for benign reasons), and the parent-HEAD binary died
        // with an illegal instruction on this exact script, so the compiled
        // path is exercised and the regression is caught.
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
        // the string-string fast path. Cut 69: `f` is straight-line, so the
        // comparison repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f(x) { var s = x + x + x; return s; } \
                     f('ab') === 'ababab'; f('ab') === 'ababab'; f('ab') === 'ababab'; \
                     f('ab') === 'ababab'; f('ab') === 'ababab'; f('ab') === 'ababab'; \
                     f('ab') === 'ababab'; f('ab') === 'ababab'; f('ab') === 'ababab'; \
                     f('ab') === 'ababab'; f('ab') === 'ababab'; f('ab') === 'ababab'; \
                     f('ab') === 'ababab'; f('ab') === 'ababab'; f('ab') === 'ababab'; \
                     f('ab') === 'ababab'; f('ab') === 'ababab';",
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
        // compile. Cut 69: `m` is straight-line, so the call repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "class A { m() { return this.v; } }\n\
                     class B extends A { constructor() { super(); this.v = 42; } m() { return super.m(); } }\n\
                     new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); \
                     new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); \
                     new B().m(); new B().m(); new B().m(); new B().m(); new B().m();",
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
                     new B().m('x', 'm'); new B().m('x', 'm'); new B().m('x', 'm'); new B().m('x', 'm'); \
                     new B().m('x', 'm'); new B().m('x', 'm'); new B().m('x', 'm'); new B().m('x', 'm'); \
                     new B().m('x', 'm'); new B().m('x', 'm'); new B().m('x', 'm'); new B().m('x', 'm'); \
                     new B().m('x', 'm'); new B().m('x', 'm'); new B().m('x', 'm'); new B().m('x', 'm'); \
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
                     new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); \
                     new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); \
                     new B().m(); new B().m(); new B().m(); new B().m(); new B().m();",
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
                     new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); \
                     new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); \
                     new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); \
                     new B().m('x'); new B().m('x');",
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
                     new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); \
                     new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); \
                     new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); new B().m('x'); \
                     new B().m('x'); new B().m('x');",
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
                     var i = 0;\n\
                     while (i++ < 17) { new B().m().then(v => { result = v; }); }",
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
                     new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); \
                     new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); new B().m(); \
                     new B().m(); new B().m(); new B().m(); new B().m(); new B().m();",
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
                     B.m() + new F().n(); B.m() + new F().n(); B.m() + new F().n(); \
                     B.m() + new F().n(); B.m() + new F().n(); B.m() + new F().n(); \
                     B.m() + new F().n(); B.m() + new F().n(); B.m() + new F().n(); \
                     B.m() + new F().n(); B.m() + new F().n(); B.m() + new F().n(); \
                     B.m() + new F().n(); B.m() + new F().n(); B.m() + new F().n(); \
                     B.m() + new F().n(); B.m() + new F().n();",
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
                     o.m(); o.m(); o.m(); o.m(); o.m(); o.m(); o.m(); o.m(); o.m(); \
                     o.m(); o.m(); o.m(); o.m(); o.m(); o.m(); o.m(); o.m();",
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
        // unset, so the read is `undefined`. Cut 69: `f` is straight-line,
        // so the comparison repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { return new.target; } \
                     f() === undefined; f() === undefined; f() === undefined; f() === undefined; \
                     f() === undefined; f() === undefined; f() === undefined; f() === undefined; \
                     f() === undefined; f() === undefined; f() === undefined; f() === undefined; \
                     f() === undefined; f() === undefined; f() === undefined; f() === undefined; \
                     f() === undefined;",
                )
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
        // proof; the method body (`m`) compiles through the JIT. Cut 69:
        // `m` is straight-line, so the expression repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function F() { this.t = new.target; }\n\
                     class C { m() { return new.target; } }\n\
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
                     (new F().t === F) && (new C().m() === undefined); \
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
        // Cut 69: `f` is straight-line, so the comparison repeats 17×.
        let (value, compiled) = with_jit_agent(|agent| {
            agent
                .run_script(
                    "function f() { return 10n; } \
                     f() === 10n; f() === 10n; f() === 10n; f() === 10n; f() === 10n; \
                     f() === 10n; f() === 10n; f() === 10n; f() === 10n; f() === 10n; \
                     f() === 10n; f() === 10n; f() === 10n; f() === 10n; f() === 10n; \
                     f() === 10n; f() === 10n;",
                )
                .expect("runs")
        });
        assert_eq!(value.as_boolean(), Some(true));
        assert!(compiled >= 1, "{compiled} bodies");
    }
}
