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
//!   `GetMemberComputed`, `AssignMemberName`/`AssignMemberComputed` (plain
//!   `=` only), and the `LeafOp` member forms.
//! - `Return` and the no-op completion steps (`ResetCompletion`,
//!   `NormalizeCompletion`, `SetCompletion`, `ListBegin`, `ListEnd` — the
//!   scaffold assumes function-body semantics, where the completion is
//!   discarded except through `Return`).
//!
//! Everything else (calls, closures, `with`/`try`/`switch`/`using`,
//! generator suspension, global-object fast paths, reference machinery,
//! per-iteration envs) bails to the interpreter.
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
//! and the scaffold allocates executable memory as RWX (W^X — a follow-up
//! should allocate RW, copy, then protect RX).

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
/// by a different body while its compiled code is cached, and entries are
/// never evicted while live — a running JIT frame holds raw pointers into
/// the executable allocation. (A future eviction policy must track
/// in-flight frames.)
pub struct JitCache {
    engine: JitEngine,
    helpers: JitHelpers,
    entries: HashMap<usize, (Rc<CompiledBody>, Rc<Compiled>)>,
}

impl JitCache {
    /// A cache whose compile step uses `helpers` as the slow-path table.
    pub fn new(helpers: JitHelpers) -> Result<Self, String> {
        Ok(Self {
            engine: JitEngine::new()?,
            helpers,
            entries: HashMap::new(),
        })
    }

    /// Look up `body`'s compiled code, compiling on first use. Returns a
    /// pointer to the metadata, valid for as long as the entry lives (the
    /// cache never evicts); null when the body is not JIT-compilable.
    pub fn lookup(&mut self, body: &Rc<CompiledBody>) -> *const JitCompiledInfo {
        let key = Rc::as_ptr(body) as usize;
        if !self.entries.contains_key(&key) {
            let Some(compiled) = self.engine.compile(body, &self.helpers) else {
                return std::ptr::null();
            };
            self.entries.insert(key, (body.clone(), Rc::new(compiled)));
        }
        // The entry is never evicted (and HashMap nodes are individually
        // heap-allocated), so the reference outlives this call.
        let (_, compiled) = &self.entries[&key];
        &compiled.info
    }

    /// The number of distinct bodies compiled so far (test introspection).
    pub fn compiled_count(&self) -> usize {
        self.entries.len()
    }
}

/// The runtime's slow-path table as a [`JitHelpers`] table: every field is
/// a real entry point, so no compiled body bails for a missing helper.
fn runtime_helpers() -> JitHelpers {
    let rt = &runtime::jit::JIT_SLOW_PATHS;
    JitHelpers {
        binary_slow: Some(rt.binary_slow),
        relational_slow: Some(rt.relational_slow),
        update_value_slow: Some(rt.update_value_slow),
        to_boolean_slow: Some(rt.to_boolean_slow),
        tdz_error: Some(rt.tdz_error),
        get_member_name: Some(rt.get_member_name),
        get_member_computed: Some(rt.get_member_computed),
        set_member_name: Some(rt.set_member_name),
        set_member_computed: Some(rt.set_member_computed),
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

unsafe extern "C" fn jit_cache_lookup(cache: *mut c_void, body: *const c_void) -> *const c_void {
    // SAFETY: the runtime passes the pointer `install` returned and a live
    // `Rc<CompiledBody>` (the caller holds it for the call).
    let cache = unsafe { &mut *(cache as *mut JitCache) };
    let body = unsafe { &*(body as *const Rc<CompiledBody>) };
    cache.lookup(body) as *const c_void
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
        }
    }

    fn helpers_all() -> JitHelpers {
        JitHelpers {
            binary_slow: Some(helpers::test_binary_slow),
            relational_slow: Some(helpers::test_relational_slow),
            update_value_slow: Some(helpers::test_update_value_slow),
            to_boolean_slow: Some(helpers::test_to_boolean_slow),
            tdz_error: Some(helpers::test_tdz_error),
            get_member_name: Some(helpers::test_get_member_name),
            get_member_computed: Some(helpers::test_get_member_computed),
            set_member_name: Some(helpers::test_set_member_name),
            set_member_computed: Some(helpers::test_set_member_computed),
        }
    }

    /// A bare (no-helpers) helper table.
    fn helpers_none() -> JitHelpers {
        JitHelpers::none()
    }

    /// Run a compiled body against a fresh frame + stack and return the
    /// completion value. Canary slots around both buffers catch out-of-bounds
    /// writes from the compiled code.
    fn run(compiled: &Compiled, frame_len: usize) -> u64 {
        const CANARY: u64 = 0xDEAD_BEEF_CAFE_F00D;
        let mut frame = vec![0u64; frame_len + 1];
        frame[frame_len] = CANARY;
        let mut stack = vec![0u64; 65];
        stack[64] = CANARY;
        // Safety: the buffers outlive the call; `vm` is never dereferenced by
        // the scaffold's test helpers.
        let result =
            unsafe { compiled.call(frame.as_mut_ptr(), stack.as_mut_ptr(), std::ptr::null_mut()) };
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
        let p1 = cache.lookup(&body);
        assert!(!p1.is_null(), "a supported body compiles");
        let p2 = cache.lookup(&body);
        assert_eq!(p1, p2, "a cached body returns the same pointer");
        // SAFETY: the cache never evicts, so the pointer stays valid.
        let info = unsafe { &*p1 };
        assert_eq!(info.stack_usage, 1, "one push above the entry stack");
        assert_ne!(info.entry, 0);
    }

    #[test]
    fn cache_returns_null_for_an_unsupported_body() {
        let mut cache = JitCache::new(helpers_all()).expect("isa");
        let body = std::rc::Rc::new(make_body(
            vec![Step::CallFast {
                argc: 0,
                direct_eval: false,
            }],
            0,
        ));
        assert!(cache.lookup(&body).is_null());
    }

    #[test]
    fn installed_jit_runs_a_certified_leaf_body() {
        // End to end through the Vm: the script body bails (its `CallFast`
        // step is unsupported), so the interpreter runs it and the
        // leaf-inline path hands the certified callee's run to the JIT. The
        // counter loop and the member access both route through the real
        // runtime slow-path table; a miscompile would surface as a wrong
        // result. The cache lives on the test's stack, so `drop_cache` is a
        // no-op and the hook is cleared before drop (the agent's Drop would
        // otherwise free a non-heap pointer).
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
        let value = agent
            .run_script(
                "function f(n) { var s = 0; for (var i = 0; i < n; i++) { s += i; } return s; }\n\
                 f(100);",
            )
            .expect("runs");
        assert_eq!(value.as_number(), Some(4950.0));
        assert!(
            cache.compiled_count() >= 1,
            "the callee body was JIT-compiled ({} bodies)",
            cache.compiled_count()
        );

        let value = agent
            .run_script("function g(o) { var v = o.x; o.x = 42; return v; } g({ x: 41 });")
            .expect("runs");
        assert_eq!(value.as_number(), Some(41.0));
        assert!(
            cache.compiled_count() >= 2,
            "g's body was JIT-compiled ({} bodies)",
            cache.compiled_count()
        );
        agent.jit_hook = None;
    }
}
