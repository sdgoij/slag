//! `Step` → CLIF lowering: the `CompiledBody` bytecode to Cranelift IR, then
//! to executable machine code.
//!
//! The scaffold's supported subset and ABI are documented on the crate root.
//! The lowering mirrors the interpreter's semantics op for op — the two
//! number fast paths (the inline tag checks are `bits & TAG_MASK !=
//! TAG_PREFIX`, two instructions on the NaN-boxed `Value`) and the slow paths
//! through the [`JitHelpers`] table. Anything unsupported bails (`None` from
//! [`JitEngine::compile`]) and the body runs on the interpreter.
//!
//! Structural notes for future work:
//! - Blocks carry no parameters; the stack pointer, loop counter, frame, and
//!   vm pointer are Cranelift variables (the SSA builder inserts the phis at
//!   seal time — blocks are sealed eagerly except back-edge targets, which
//!   `seal_all_blocks` closes at the end).
//! - Branches use `brif` (both targets explicit), so a fast/slow merge is two
//!   blocks joined by a variable both paths define — no forwarder blocks.
//! - The fused canonical loop (`FastLoopHead`/`RunRegBody`/`PushAcc`/...) is
//!   lowered to real branches with the counter in an f64 register, mirroring
//!   `fast_loop_inc`/`fast_loop_test` exactly (a `FastLoopVar::Counter` head
//!   is number-only by the acc-path gate; a `Slot` head keeps the
//!   number-check + slow-path fallback).

use std::collections::HashSet;
use std::sync::Arc;

use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::Offset32;
use cranelift_codegen::ir::{
    AbiParam, Block, Function, InstBuilder, MemFlagsData, SigRef, Signature, TrapCode, Type,
    UserFuncName, Value as ClifValue, types,
};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig, TargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_control::ControlPlane;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use crux::Value;
use runtime::ir::{CompiledBody, FastLoopVar, LeafOp, RegOperand, ScopeInfo, Step};
use syntax::ast::{AssignOp, BinaryOp, UpdateOp};
use target_lexicon::PointerWidth;

use crate::helpers::{Helper, JitHelpers};
use crate::{Compiled, ExecutableCode, JitEntry};

/// The native-code generation engine: a native `TargetIsa` plus reusable
/// compilation contexts.
pub struct JitEngine {
    isa: Arc<dyn TargetIsa>,
}

impl JitEngine {
    /// Build a native ISA (host triple + `opt_level=speed`).
    pub fn new() -> Result<Self, String> {
        let mut flag_builder = settings::builder();
        flag_builder
            .set("opt_level", "speed")
            .map_err(|e| e.to_string())?;
        let flags = settings::Flags::new(flag_builder);
        let isa = cranelift_native::builder()?
            .finish(flags)
            .map_err(|e| e.to_string())?;
        Ok(Self { isa })
    }

    /// Compile `body` to executable machine code, or `None` when the body
    /// contains a step outside the supported subset or needs a slow-path
    /// helper that is missing from `helpers`.
    pub fn compile(&self, body: &CompiledBody, helpers: &JitHelpers) -> Option<Compiled> {
        let conv = platform_call_conv(&*self.isa);
        let mut func =
            Function::with_name_signature(UserFuncName::testcase("jit_body"), jit_sig(conv));
        let mut fctx = FunctionBuilderContext::new();
        lower(body, helpers, &mut func, &mut fctx, &*self.isa, conv).ok()?;
        if std::env::var("JIT_DUMP_CLIF").is_ok() {
            eprintln!("{} \n", func.display());
        }
        let mut ctx = Context::for_function(func);
        if std::env::var("JIT_DUMP_CLIF").is_ok() {
            ctx.set_disasm(true);
        }
        let compiled = ctx.compile(&*self.isa, &mut ControlPlane::default()).ok()?;
        if let Some(disasm) = compiled.vcode.as_ref() {
            eprintln!("--- disasm ---\n{disasm}");
        }
        let code = ExecutableCode::new(compiled.code_buffer()).ok()?;
        // SAFETY: the allocation outlives the cast and is executable; a data
        // pointer to a fn pointer is a plain integer cast on every supported
        // target (no trampolines).
        let entry: JitEntry = unsafe { std::mem::transmute(code.as_ptr()) };
        Some(Compiled { code, entry })
    }
}

/// The C ABI for the host platform: the JIT entry and the slow-path helpers
/// are `extern "C"` Rust functions, so the compiled code must use the same
/// convention. Cranelift's `CallConv::Fast` is an internal, non-ABI-stable
/// convention (System V registers even on Windows), which would pass the
/// frame/stack/vm arguments in the wrong registers.
fn platform_call_conv(isa: &dyn TargetIsa) -> CallConv {
    if isa.triple().operating_system == target_lexicon::OperatingSystem::Windows {
        CallConv::WindowsFastcall
    } else {
        CallConv::SystemV
    }
}

/// The JIT entry signature: `(frame, stack, vm) -> completion value`.
fn jit_sig(conv: CallConv) -> Signature {
    let mut sig = Signature::new(conv);
    sig.params.push(AbiParam::new(types::I64)); // frame
    sig.params.push(AbiParam::new(types::I64)); // stack
    sig.params.push(AbiParam::new(types::I64)); // vm
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

/// A slow-path helper signature (all helpers return one I64 value).
fn helper_sig(params: &[Type], conv: CallConv) -> Signature {
    let mut sig = Signature::new(conv);
    sig.params.extend(params.iter().copied().map(AbiParam::new));
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

/// Lower `body` into `func`. `func`/`fctx` are consumed by the builder and
/// finalized in place.
fn lower<'a>(
    body: &'a CompiledBody,
    helpers: &'a JitHelpers,
    func: &'a mut Function,
    fctx: &'a mut FunctionBuilderContext,
    isa: &dyn TargetIsa,
    conv: CallConv,
) -> Result<(), Unsupported> {
    let mut lowerer = Lowerer::new(body, helpers, func, fctx, conv);
    lowerer.emit_all(body)?;
    lowerer.builder.seal_all_blocks();
    lowerer.builder.finalize(TargetFrontendConfig {
        default_call_conv: conv,
        pointer_width: PointerWidth::U64,
        page_size_align_log2: isa.page_size_align_log2(),
    });
    Ok(())
}

/// Why a body cannot be JIT-compiled (fall back to the interpreter). The
/// payloads are diagnostic strings for the integration's logging; the
/// scaffold's `compile` currently swallows the error (`Option`).
#[allow(dead_code)]
#[derive(Debug)]
enum Unsupported {
    Step(&'static str),
    Leaf(&'static str),
    Helper(&'static str),
    Const(&'static str),
}

/// The arithmetic ops the JIT inlines for two numbers. `Rem` and `Exp` are
/// NOT here: cranelift 0.134 has no `frem`/`fpow` instructions, and a formula
/// reimplementation would not match Rust's `%`/`powf` bit for bit, so those
/// route through `binary_slow`.
#[derive(Clone, Copy)]
enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Copy)]
enum InlineBin {
    Arith(ArithOp),
    Cmp(FloatCC),
}

/// The number-number shapes `binary_inline` inlines in the interpreter
/// (`apply_binary` for two numbers). `Equal`/`StrictEqual` on two numbers are
/// the same f64 compare (NaN != NaN, -0 == 0, matching JS).
fn inline_binary(op: BinaryOp) -> Option<InlineBin> {
    use BinaryOp::*;
    match op {
        Add => Some(InlineBin::Arith(ArithOp::Add)),
        Sub => Some(InlineBin::Arith(ArithOp::Sub)),
        Mul => Some(InlineBin::Arith(ArithOp::Mul)),
        Div => Some(InlineBin::Arith(ArithOp::Div)),
        LessThan => Some(InlineBin::Cmp(FloatCC::LessThan)),
        GreaterThan => Some(InlineBin::Cmp(FloatCC::GreaterThan)),
        LessEqual => Some(InlineBin::Cmp(FloatCC::LessThanOrEqual)),
        GreaterEqual => Some(InlineBin::Cmp(FloatCC::GreaterThanOrEqual)),
        Equal | StrictEqual => Some(InlineBin::Cmp(FloatCC::Equal)),
        NotEqual | StrictNotEqual => Some(InlineBin::Cmp(FloatCC::NotEqual)),
        _ => None,
    }
}

fn rel_cc(op: BinaryOp) -> Result<FloatCC, Unsupported> {
    match op {
        BinaryOp::LessThan => Ok(FloatCC::LessThan),
        BinaryOp::LessEqual => Ok(FloatCC::LessThanOrEqual),
        BinaryOp::GreaterThan => Ok(FloatCC::GreaterThan),
        BinaryOp::GreaterEqual => Ok(FloatCC::GreaterThanOrEqual),
        _ => Err(Unsupported::Step("non-relational loop test")),
    }
}

/// A short diagnostic name for a step that bailed.
fn step_name(step: &Step) -> &'static str {
    match step {
        Step::Call { .. } | Step::CallFast { .. } => "Call",
        Step::CallFastGlobal { .. } | Step::CallFastSlot { .. } => "CallFastGlobal/Slot",
        Step::TailCall { .. } | Step::TailCallFast { .. } => "TailCall",
        Step::Throw => "Throw",
        Step::LoadIdent { .. } => "LoadIdent",
        Step::Unary(_) => "Unary",
        Step::EnterTry { .. } => "EnterTry",
        Step::EnterWith => "EnterWith",
        Step::CreateFunction { .. } | Step::CreateArrow { .. } => "CreateFunction",
        Step::LoadGlobal { .. } | Step::StoreGlobal { .. } | Step::UpdateGlobal { .. } => {
            "Global fast path"
        }
        Step::ForOfNext { .. } | Step::ForInNext { .. } | Step::ForOfBegin { .. } => "ForOf/ForIn",
        Step::SwitchDisc | Step::SwitchTest { .. } => "Switch",
        Step::Break { .. } | Step::Continue { .. } => "Break/Continue",
        Step::Yield { .. } | Step::Await => "Yield/Await",
        Step::ImportCall { .. } | Step::ImportMeta => "Import",
        Step::ResolveVarIdent { .. } | Step::GetVarReference => "Reference machinery",
        Step::GetSuperName { .. } | Step::GetSuperBase => "Super",
        _ => "unsupported step",
    }
}

/// The jump targets of a step (for back-edge detection).
fn step_targets(step: &Step) -> Vec<usize> {
    match step {
        Step::Jump(t)
        | Step::JumpIfFalse(t)
        | Step::JumpIfTrue(t)
        | Step::JumpIfFalseKeep(t)
        | Step::JumpIfTrueKeep(t)
        | Step::JumpIfNullishKeep(t)
        | Step::JumpIfNotNullishKeep(t)
        | Step::Break { target: t }
        | Step::Continue { target: t } => vec![*t],
        Step::JumpIfLtImm { target, .. }
        | Step::JumpIfLeImm { target, .. }
        | Step::JumpIfGtImm { target, .. }
        | Step::JumpIfGeImm { target, .. }
        | Step::JumpIfLtGlobalImm { target, .. }
        | Step::JumpIfLeGlobalImm { target, .. }
        | Step::JumpIfGtGlobalImm { target, .. }
        | Step::JumpIfGeGlobalImm { target, .. } => vec![*target],
        Step::FastLoopHead {
            body_start, after, ..
        } => vec![*body_start, *after],
        Step::JumpIfChainShort(t) => vec![*t],
        Step::Exit { after } => vec![*after],
        Step::ForInNext { back, .. } | Step::ForOfNext { back, .. } => vec![*back],
        Step::ForOfNextBindLocal { back, .. } => vec![*back],
        _ => Vec::new(),
    }
}

struct Lowerer<'a> {
    builder: FunctionBuilder<'a>,
    helpers: &'a JitHelpers,
    scope: Option<&'a ScopeInfo>,
    /// One block per step index, plus the past-the-end exit block.
    blocks: Vec<Option<Block>>,
    /// Blocks that receive a jump from a LATER step (back edges) — sealed at
    /// the end by `seal_all_blocks`, not at visit time.
    back_targets: HashSet<usize>,
    frame_var: Variable,
    sp_var: Variable,
    vm_var: Variable,
    counter_var: Variable,
    acc_var: Variable,
    sig_binary: SigRef,
    sig_rel: SigRef,
    sig_update: SigRef,
    sig_bool: SigRef,
    sig_tdz: SigRef,
    sig_get_name: SigRef,
    sig_get_comp: SigRef,
    sig_set_name: SigRef,
    sig_set_comp: SigRef,
    // NaN-boxing bit patterns (see `crux::value`).
    undef_bits: i64,
    null_bits: i64,
    false_bits: i64,
    true_bits: i64,
    uninit_bits: i64,
    canon_nan_bits: i64,
}

impl<'a> Lowerer<'a> {
    fn new(
        body: &'a CompiledBody,
        helpers: &'a JitHelpers,
        func: &'a mut Function,
        fctx: &'a mut FunctionBuilderContext,
        conv: CallConv,
    ) -> Self {
        let mut builder = FunctionBuilder::new(func, fctx);
        let frame_var = builder.declare_var(types::I64);
        let sp_var = builder.declare_var(types::I64);
        let vm_var = builder.declare_var(types::I64);
        let counter_var = builder.declare_var(types::F64);
        let acc_var = builder.declare_var(types::I64);
        let sig_binary = builder.import_signature(helper_sig(&[types::I64; 4], conv));
        let sig_rel = builder.import_signature(helper_sig(&[types::I64; 4], conv));
        let sig_update = builder.import_signature(helper_sig(&[types::I64; 3], conv));
        let sig_bool = builder.import_signature(helper_sig(&[types::I64; 2], conv));
        let sig_tdz = builder.import_signature(helper_sig(&[types::I64; 1], conv));
        let sig_get_name = builder.import_signature(helper_sig(&[types::I64; 3], conv));
        let sig_get_comp = builder.import_signature(helper_sig(&[types::I64; 3], conv));
        let sig_set_name = builder.import_signature(helper_sig(&[types::I64; 4], conv));
        let sig_set_comp = builder.import_signature(helper_sig(&[types::I64; 4], conv));
        Lowerer {
            builder,
            helpers,
            scope: body.scope.as_ref(),
            blocks: Vec::new(),
            back_targets: HashSet::new(),
            frame_var,
            sp_var,
            vm_var,
            counter_var,
            acc_var,
            sig_binary,
            sig_rel,
            sig_update,
            sig_bool,
            sig_tdz,
            sig_get_name,
            sig_get_comp,
            sig_set_name,
            sig_set_comp,
            undef_bits: Value::Undefined.bits() as i64,
            null_bits: Value::Null.bits() as i64,
            false_bits: Value::Boolean(false).bits() as i64,
            true_bits: Value::Boolean(true).bits() as i64,
            uninit_bits: Value::uninitialized().bits() as i64,
            canon_nan_bits: Value::Number(f64::NAN).bits() as i64,
        }
    }

    // ----- blocks and the step walk -----

    fn ensure_block(&mut self, index: usize) -> Block {
        if let Some(block) = self.blocks[index] {
            return block;
        }
        let block = self.builder.create_block();
        if index == 0 {
            // The entry block's parameters ARE the function parameters.
            self.builder.append_block_params_for_function_params(block);
        }
        self.blocks[index] = Some(block);
        block
    }

    /// Switch to step `index`'s block. Only the entry block has parameters
    /// (the function's); every other block's variables flow in through the
    /// SSA machinery. Seals the block unless a later step jumps back to it.
    fn visit(&mut self, index: usize) {
        let block = self.ensure_block(index);
        self.builder.switch_to_block(block);
        if index == 0 {
            let params = self.builder.block_params(block).to_vec();
            self.builder.def_var(self.frame_var, params[0]);
            self.builder.def_var(self.sp_var, params[1]);
            self.builder.def_var(self.vm_var, params[2]);
        }
        if !self.back_targets.contains(&index) {
            self.builder.seal_block(block);
        }
    }

    /// Fall through from step `index` to the next step's block.
    fn fall_through(&mut self, index: usize) {
        let next = self.ensure_block(index + 1);
        self.builder.ins().jump(next, &[]);
    }

    /// End the current block with a branch on `cond` (0/1, any int width):
    /// to `target_block` when `jump_when` is true, else to `next`.
    fn cond_jump(&mut self, cond: ClifValue, jump_when: bool, target_block: Block, next: usize) {
        let next_block = self.ensure_block(next);
        let cond = self.builder.ins().icmp_imm_u(IntCC::NotEqual, cond, 0);
        if jump_when {
            self.builder
                .ins()
                .brif(cond, target_block, &[], next_block, &[]);
        } else {
            self.builder
                .ins()
                .brif(cond, next_block, &[], target_block, &[]);
        }
    }

    fn emit_all(&mut self, body: &CompiledBody) -> Result<(), Unsupported> {
        let n = body.steps.len();
        self.blocks = vec![None; n + 1];
        self.back_targets.clear();
        for (index, step) in body.steps.iter().enumerate() {
            for target in step_targets(step) {
                if target < index {
                    self.back_targets.insert(target);
                }
            }
        }
        // Entry block: bind the parameters; seed the scratch variables so a
        // malformed body can never read an undefined variable (the compiler
        // only emits counter/acc uses inside counter loops / register bodies).
        self.visit(0);
        let zero = self.builder.ins().f64const(0.0);
        self.builder.def_var(self.counter_var, zero);
        let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
        self.builder.def_var(self.acc_var, undef);
        for (index, step) in body.steps.iter().enumerate() {
            if index > 0 {
                self.visit(index);
            }
            self.emit_step(index, step)?;
        }
        // Past-the-end block: a body that falls off completes `Empty`, which
        // leaf callers observe as `undefined`.
        self.visit(n);
        let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
        self.builder.ins().return_(&[undef]);
        Ok(())
    }

    // ----- value stack and frame -----

    fn push(&mut self, value: ClifValue) {
        let sp = self.builder.use_var(self.sp_var);
        self.builder
            .ins()
            .store(MemFlagsData::new(), value, sp, Offset32::new(0));
        let next = self.builder.ins().iadd_imm_s(sp, 8);
        self.builder.def_var(self.sp_var, next);
    }

    fn pop(&mut self) -> ClifValue {
        let sp = self.builder.use_var(self.sp_var);
        let prev = self.builder.ins().iadd_imm_s(sp, -8);
        self.builder.def_var(self.sp_var, prev);
        self.builder
            .ins()
            .load(types::I64, MemFlagsData::new(), prev, Offset32::new(0))
    }

    /// Peek the top of the stack without popping.
    fn top(&mut self) -> ClifValue {
        let sp = self.builder.use_var(self.sp_var);
        let top = self.builder.ins().iadd_imm_s(sp, -8);
        self.builder
            .ins()
            .load(types::I64, MemFlagsData::new(), top, Offset32::new(0))
    }

    fn dup(&mut self) {
        let sp = self.builder.use_var(self.sp_var);
        let top = self.builder.ins().iadd_imm_s(sp, -8);
        let value = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), top, Offset32::new(0));
        self.builder
            .ins()
            .store(MemFlagsData::new(), value, sp, Offset32::new(0));
        let next = self.builder.ins().iadd_imm_s(sp, 8);
        self.builder.def_var(self.sp_var, next);
    }

    fn load_slot(&mut self, slot: usize) -> ClifValue {
        let frame = self.builder.use_var(self.frame_var);
        self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            frame,
            Offset32::new((slot as i32) * 8),
        )
    }

    fn store_slot(&mut self, slot: usize, value: ClifValue) {
        let frame = self.builder.use_var(self.frame_var);
        self.builder.ins().store(
            MemFlagsData::new(),
            value,
            frame,
            Offset32::new((slot as i32) * 8),
        );
    }

    /// Whether the slot holds a lexical binding (`ScopeInfo::tdz_store` marks
    /// `let`/`const` slots; params/`var`s are never the uninitialized marker,
    /// so only lexical slots need the marker check).
    fn slot_is_lexical(&self, slot: usize) -> bool {
        self.scope
            .and_then(|s| s.tdz_store.get(slot).copied())
            .unwrap_or(false)
    }

    // ----- helpers -----

    fn vm(&mut self) -> ClifValue {
        self.builder.use_var(self.vm_var)
    }

    fn call_slow(
        &mut self,
        sig: SigRef,
        helper: Helper,
        args: &[ClifValue],
    ) -> Result<ClifValue, Unsupported> {
        let f = self
            .helpers
            .get(helper)
            .ok_or(Unsupported::Helper(helper.name()))?;
        let callee = self.builder.ins().iconst(types::I64, f as i64);
        // The vm pointer is the implicit first argument of every helper.
        let mut all_args = Vec::with_capacity(args.len() + 1);
        all_args.push(self.vm());
        all_args.extend_from_slice(args);
        let inst = self.builder.ins().call_indirect(sig, callee, &all_args);
        Ok(self.builder.func.dfg.inst_results(inst)[0])
    }

    fn const_value(&mut self, value: &Value) -> Result<ClifValue, Unsupported> {
        if value.is_undefined() {
            Ok(self.builder.ins().iconst(types::I64, self.undef_bits))
        } else if value.is_null() {
            Ok(self.builder.ins().iconst(types::I64, self.null_bits))
        } else if value.is_boolean() {
            let bits = if value.as_boolean().unwrap_or(false) {
                self.true_bits
            } else {
                self.false_bits
            };
            Ok(self.builder.ins().iconst(types::I64, bits))
        } else if let Some(n) = value.as_number() {
            Ok(self
                .builder
                .ins()
                .iconst(types::I64, Value::Number(n).bits() as i64))
        } else {
            Err(Unsupported::Const("heap value literal"))
        }
    }

    // ----- inline kernels -----

    /// `bits & TAG_MASK != TAG_PREFIX` — the double check (I8).
    fn is_double(&mut self, bits: ClifValue) -> ClifValue {
        let masked = self.builder.ins().band_imm_u(bits, crux::TAG_MASK as i64);
        self.builder
            .ins()
            .icmp_imm_u(IntCC::NotEqual, masked, crux::TAG_PREFIX as i64)
    }

    /// I8 bool → I64 0/1 (cranelift 0.134's `bint` is `uextend`).
    fn bint(&mut self, cond: ClifValue) -> ClifValue {
        self.builder.ins().uextend(types::I64, cond)
    }

    /// Canonicalize a computed double's bits: a quiet NaN whose top 16 bits
    /// collide with the tag region would read as a tag — replace it with the
    /// canonical NaN, exactly like `Value::Number`.
    fn canon(&mut self, bits: ClifValue) -> ClifValue {
        let masked = self.builder.ins().band_imm_u(bits, crux::TAG_MASK as i64);
        let collides = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, masked, crux::TAG_PREFIX as i64);
        let canon = self.builder.ins().iconst(types::I64, self.canon_nan_bits);
        self.builder.ins().select(collides, canon, bits)
    }

    /// The loop counter as a `Value` (`Value::Number(f64)` with the NaN
    /// canonicalization).
    fn counter_bits(&mut self) -> ClifValue {
        let cur = self.builder.use_var(self.counter_var);
        let bits = self
            .builder
            .ins()
            .bitcast(types::I64, MemFlagsData::new(), cur);
        self.canon(bits)
    }

    fn inc_counter(&mut self, delta: f64) {
        let cur = self.builder.use_var(self.counter_var);
        let d = self.builder.ins().f64const(delta);
        let next = self.builder.ins().fadd(cur, d);
        self.builder.def_var(self.counter_var, next);
    }

    // ----- steps -----

    fn emit_step(&mut self, index: usize, step: &Step) -> Result<(), Unsupported> {
        match step {
            Step::Push(value) => {
                let bits = self.const_value(value)?;
                self.push(bits);
                self.fall_through(index);
            }
            Step::Pop => {
                self.pop();
                self.fall_through(index);
            }
            Step::Dup => {
                self.dup();
                self.fall_through(index);
            }
            Step::Binary(op) => {
                let rhs = self.pop();
                let lhs = self.pop();
                let res = self.emit_binary(*op, lhs, rhs)?;
                self.push(res);
                self.fall_through(index);
            }
            Step::BinaryImm { op, imm } => {
                let lhs = self.pop();
                let rhs = self.const_value(&Value::Number(*imm))?;
                let res = self.emit_binary(*op, lhs, rhs)?;
                self.push(res);
                self.fall_through(index);
            }
            Step::LoadLocal { slot } => {
                let bits = self.load_slot(*slot);
                if self.slot_is_lexical(*slot) {
                    self.emit_tdz_check(bits)?;
                }
                self.push(bits);
                self.fall_through(index);
            }
            Step::StoreLocal { slot } | Step::FusedStoreLocal { slot } => {
                let value = self.pop();
                if self.slot_is_lexical(*slot) {
                    let current = self.load_slot(*slot);
                    self.emit_tdz_check(current)?;
                }
                self.store_slot(*slot, value);
                self.fall_through(index);
            }
            Step::InitLocal { slot } => {
                let value = self.pop();
                self.store_slot(*slot, value);
                self.fall_through(index);
            }
            Step::Inc { slot } => {
                self.emit_update(*slot, UpdateOp::Increment, false, false, index)?
            }
            Step::Dec { slot } => {
                self.emit_update(*slot, UpdateOp::Decrement, false, false, index)?
            }
            Step::UpdateLocal { slot, op, prefix } => {
                self.emit_update(*slot, *op, *prefix, true, index)?
            }
            Step::Jump(target) => {
                let block = self.ensure_block(*target);
                self.builder.ins().jump(block, &[]);
            }
            Step::JumpIfFalse(target) => {
                let bits = self.pop();
                let falsy = self.emit_truthiness(bits)?;
                let block = self.ensure_block(*target);
                self.cond_jump(falsy, true, block, index + 1);
            }
            Step::JumpIfTrue(target) => {
                let bits = self.pop();
                let falsy = self.emit_truthiness(bits)?;
                let block = self.ensure_block(*target);
                self.cond_jump(falsy, false, block, index + 1);
            }
            Step::JumpIfFalseKeep(target) => {
                let bits = self.top();
                let falsy = self.emit_truthiness(bits)?;
                let block = self.ensure_block(*target);
                self.cond_jump(falsy, true, block, index + 1);
            }
            Step::JumpIfTrueKeep(target) => {
                let bits = self.top();
                let falsy = self.emit_truthiness(bits)?;
                let block = self.ensure_block(*target);
                self.cond_jump(falsy, false, block, index + 1);
            }
            Step::JumpIfNullishKeep(target) => {
                let bits = self.top();
                let nullish = self.emit_nullish(bits);
                let block = self.ensure_block(*target);
                self.cond_jump(nullish, true, block, index + 1);
            }
            Step::JumpIfNotNullishKeep(target) => {
                let bits = self.top();
                let nullish = self.emit_nullish(bits);
                let block = self.ensure_block(*target);
                self.cond_jump(nullish, false, block, index + 1);
            }
            Step::JumpIfLtImm { slot, imm, target } => {
                self.emit_rel_test_jump(*slot, *imm, BinaryOp::LessThan, *target, index + 1)?
            }
            Step::JumpIfLeImm { slot, imm, target } => {
                self.emit_rel_test_jump(*slot, *imm, BinaryOp::LessEqual, *target, index + 1)?
            }
            Step::JumpIfGtImm { slot, imm, target } => {
                self.emit_rel_test_jump(*slot, *imm, BinaryOp::GreaterThan, *target, index + 1)?
            }
            Step::JumpIfGeImm { slot, imm, target } => {
                self.emit_rel_test_jump(*slot, *imm, BinaryOp::GreaterEqual, *target, index + 1)?
            }
            Step::FastLoopBind { var } => {
                self.emit_fast_loop_bind(*var)?;
                self.fall_through(index);
            }
            Step::FastLoopStore { var } => {
                self.emit_fast_loop_store(*var)?;
                self.fall_through(index);
            }
            Step::FastLoopHead {
                var,
                op,
                imm,
                inc,
                body_start,
                after,
            } => self.emit_fast_loop_head(*var, *op, *imm, *inc, *body_start, *after)?,
            Step::RunRegBody { ops } => {
                let entry_sp = self.builder.use_var(self.sp_var);
                let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
                self.builder.def_var(self.acc_var, undef);
                let returns = matches!(ops.last(), Some(LeafOp::ReturnAcc));
                for op in ops.iter() {
                    self.emit_leaf_op(op)?;
                }
                if returns {
                    // `ReturnAcc` already terminated the block.
                    return Ok(());
                }
                // The interpreter truncates the register body's transient
                // stack use to the entry length.
                self.builder.def_var(self.sp_var, entry_sp);
                self.fall_through(index);
            }
            Step::PushAcc => {
                let bits = self.counter_bits();
                self.push(bits);
                self.fall_through(index);
            }
            Step::PopAcc => {
                let bits = self.pop();
                let num = self
                    .builder
                    .ins()
                    .bitcast(types::F64, MemFlagsData::new(), bits);
                let is_num = self.is_double(bits);
                let zero = self.builder.ins().f64const(0.0);
                let sel = self.builder.ins().select(is_num, num, zero);
                self.builder.def_var(self.counter_var, sel);
                self.fall_through(index);
            }
            Step::IncAcc => {
                self.inc_counter(1.0);
                self.fall_through(index);
            }
            Step::DecAcc => {
                self.inc_counter(-1.0);
                self.fall_through(index);
            }
            Step::GetMemberName { name } => {
                let object = self.pop();
                let name = self.builder.ins().iconst(types::I64, *name as i64);
                let res =
                    self.call_slow(self.sig_get_name, Helper::GetMemberName, &[object, name])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::GetMemberComputed => {
                let key = self.pop();
                let object = self.pop();
                let res =
                    self.call_slow(self.sig_get_comp, Helper::GetMemberComputed, &[object, key])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::AssignMemberName { name, op } => {
                if !matches!(op, AssignOp::Assign) {
                    return Err(Unsupported::Step("AssignMemberName compound"));
                }
                let value = self.pop();
                let object = self.pop();
                let name = self.builder.ins().iconst(types::I64, *name as i64);
                let res = self.call_slow(
                    self.sig_set_name,
                    Helper::SetMemberName,
                    &[object, name, value],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::AssignMemberComputed { op } => {
                if !matches!(op, AssignOp::Assign) {
                    return Err(Unsupported::Step("AssignMemberComputed compound"));
                }
                let value = self.pop();
                let key = self.pop();
                let object = self.pop();
                let res = self.call_slow(
                    self.sig_set_comp,
                    Helper::SetMemberComputed,
                    &[object, key, value],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::Return => {
                let value = self.pop();
                self.builder.ins().return_(&[value]);
            }
            // Completion bookkeeping: the scaffold assumes function-body
            // semantics, where the completion value is only observable
            // through `Return` (a statement-list completion is eval/script
            // territory, not yet JIT-compiled).
            Step::ResetCompletion
            | Step::NormalizeCompletion
            | Step::SetCompletion
            | Step::ListBegin
            | Step::ListEnd => {
                self.fall_through(index);
            }
            _ => return Err(Unsupported::Step(step_name(step))),
        }
        Ok(())
    }

    /// The fused `slot <op> imm` loop test: `emit_rel_test` then jump to
    /// `target` when the test is FALSE (the loop exit).
    fn emit_rel_test_jump(
        &mut self,
        slot: usize,
        imm: f64,
        op: BinaryOp,
        target: usize,
        next: usize,
    ) -> Result<(), Unsupported> {
        let bits = self.load_slot(slot);
        let test = self.emit_rel_test(op, bits, imm)?;
        let block = self.ensure_block(target);
        self.cond_jump(test, false, block, next);
        Ok(())
    }

    /// The JS relational test `value <op> imm` as a 0/1 I64: the number fast
    /// path inline, the non-Number path through `relational_slow`.
    fn emit_rel_test(
        &mut self,
        op: BinaryOp,
        bits: ClifValue,
        imm: f64,
    ) -> Result<ClifValue, Unsupported> {
        let cc = rel_cc(op)?;
        let is_num = self.is_double(bits);
        let num = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), bits);
        let imm_num = self.builder.ins().f64const(imm);
        let cmp = self.builder.ins().fcmp(cc, num, imm_num);
        let test_fast = self.bint(cmp);
        let imm_bits = self.const_value(&Value::Number(imm))?;
        let test_var = self.builder.declare_var(types::I64);
        self.builder.def_var(test_var, test_fast);
        let merge = self.builder.create_block();
        let slow = self.builder.create_block();
        self.builder.ins().brif(is_num, merge, &[], slow, &[]);
        self.builder.switch_to_block(slow);
        let op_imm = self.builder.ins().iconst(types::I64, op as i64);
        let test_slow = self.call_slow(
            self.sig_rel,
            Helper::RelationalSlow,
            &[op_imm, bits, imm_bits],
        )?;
        self.builder.def_var(test_var, test_slow);
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(merge);
        self.builder.switch_to_block(merge);
        Ok(self.builder.use_var(test_var))
    }

    fn emit_fast_loop_bind(&mut self, var: FastLoopVar) -> Result<(), Unsupported> {
        match var {
            // The counter prologue: load the binding's Number into the f64
            // counter (the acc-path gate guarantees a Number; the fallback
            // matches `fast_loop_bind`'s release-mode `unwrap_or(0.0)`).
            FastLoopVar::Slot(slot) => {
                let bits = self.load_slot(slot);
                let num = self
                    .builder
                    .ins()
                    .bitcast(types::F64, MemFlagsData::new(), bits);
                let is_num = self.is_double(bits);
                let zero = self.builder.ins().f64const(0.0);
                let sel = self.builder.ins().select(is_num, num, zero);
                self.builder.def_var(self.counter_var, sel);
                Ok(())
            }
            FastLoopVar::Global(_) => Err(Unsupported::Step("FastLoopBind Global")),
            FastLoopVar::Counter => Err(Unsupported::Step("FastLoopBind Counter")),
        }
    }

    fn emit_fast_loop_store(&mut self, var: FastLoopVar) -> Result<(), Unsupported> {
        match var {
            FastLoopVar::Slot(slot) => {
                let value = self.counter_bits();
                self.store_slot(slot, value);
                Ok(())
            }
            FastLoopVar::Global(_) => Err(Unsupported::Step("FastLoopStore Global")),
            FastLoopVar::Counter => Err(Unsupported::Step("FastLoopStore Counter")),
        }
    }

    /// The fused canonical loop's back edge: increment, re-test, branch back
    /// to `body_start` (pass) or `after` (fail).
    fn emit_fast_loop_head(
        &mut self,
        var: FastLoopVar,
        op: BinaryOp,
        imm: f64,
        inc: UpdateOp,
        body_start: usize,
        after: usize,
    ) -> Result<(), Unsupported> {
        let body_block = self.ensure_block(body_start);
        match var {
            // The accumulator counter is a raw f64 — the number test inline
            // (the acc-path gate admits only Number inits/stores).
            FastLoopVar::Counter => {
                let delta = if matches!(inc, UpdateOp::Increment) {
                    1.0
                } else {
                    -1.0
                };
                self.inc_counter(delta);
                let cur = self.builder.use_var(self.counter_var);
                let cc = rel_cc(op)?;
                let imm_num = self.builder.ins().f64const(imm);
                let cmp = self.builder.ins().fcmp(cc, cur, imm_num);
                let test = self.bint(cmp);
                self.cond_jump(test, true, body_block, after);
                Ok(())
            }
            // A frame-slot counter may be a non-Number: `fast_loop_inc`'s
            // general path plus `fast_loop_test`'s relational fallback.
            FastLoopVar::Slot(slot) => {
                let old = self.load_slot(slot);
                let is_num = self.is_double(old);
                let num = self
                    .builder
                    .ins()
                    .bitcast(types::F64, MemFlagsData::new(), old);
                let delta = if matches!(inc, UpdateOp::Increment) {
                    1.0
                } else {
                    -1.0
                };
                let delta_c = self.builder.ins().f64const(delta);
                let new_num = self.builder.ins().fadd(num, delta_c);
                let new_bits = self
                    .builder
                    .ins()
                    .bitcast(types::I64, MemFlagsData::new(), new_num);
                let new_fast = self.canon(new_bits);
                let cc = rel_cc(op)?;
                let imm_num = self.builder.ins().f64const(imm);
                let cmp = self.builder.ins().fcmp(cc, new_num, imm_num);
                let test_fast = self.bint(cmp);
                let imm_bits = self.const_value(&Value::Number(imm))?;
                let new_var = self.builder.declare_var(types::I64);
                let test_var = self.builder.declare_var(types::I64);
                self.builder.def_var(new_var, new_fast);
                self.builder.def_var(test_var, test_fast);
                let merge = self.builder.create_block();
                let slow = self.builder.create_block();
                self.builder.ins().brif(is_num, merge, &[], slow, &[]);
                self.builder.switch_to_block(slow);
                let inc_imm = self.builder.ins().iconst(types::I64, inc as i64);
                let new_slow =
                    self.call_slow(self.sig_update, Helper::UpdateValueSlow, &[inc_imm, old])?;
                let op_imm = self.builder.ins().iconst(types::I64, op as i64);
                let test_slow = self.call_slow(
                    self.sig_rel,
                    Helper::RelationalSlow,
                    &[op_imm, new_slow, imm_bits],
                )?;
                self.builder.def_var(new_var, new_slow);
                self.builder.def_var(test_var, test_slow);
                self.builder.ins().jump(merge, &[]);
                self.builder.seal_block(merge);
                self.builder.switch_to_block(merge);
                let new_value = self.builder.use_var(new_var);
                let test = self.builder.use_var(test_var);
                self.store_slot(slot, new_value);
                self.cond_jump(test, true, body_block, after);
                Ok(())
            }
            FastLoopVar::Global(_) => Err(Unsupported::Step("FastLoopHead Global")),
        }
    }

    /// `Inc`/`Dec`/`UpdateLocal`: read the slot, apply `++`/`--`, store, and
    /// optionally push the result (old for postfix, new for prefix — the
    /// value-discarding forms push nothing).
    fn emit_update(
        &mut self,
        slot: usize,
        op: UpdateOp,
        prefix: bool,
        push_result: bool,
        index: usize,
    ) -> Result<(), Unsupported> {
        let old = self.load_slot(slot);
        if self.slot_is_lexical(slot) {
            self.emit_tdz_check(old)?;
        }
        let is_num = self.is_double(old);
        let num = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), old);
        let delta = if matches!(op, UpdateOp::Increment) {
            1.0
        } else {
            -1.0
        };
        let delta_c = self.builder.ins().f64const(delta);
        let new_num = self.builder.ins().fadd(num, delta_c);
        let new_bits = self
            .builder
            .ins()
            .bitcast(types::I64, MemFlagsData::new(), new_num);
        let new_fast = self.canon(new_bits);
        let new_var = self.builder.declare_var(types::I64);
        self.builder.def_var(new_var, new_fast);
        let merge = self.builder.create_block();
        let slow = self.builder.create_block();
        self.builder.ins().brif(is_num, merge, &[], slow, &[]);
        self.builder.switch_to_block(slow);
        let inc_imm = self.builder.ins().iconst(types::I64, op as i64);
        let new_slow = self.call_slow(self.sig_update, Helper::UpdateValueSlow, &[inc_imm, old])?;
        self.builder.def_var(new_var, new_slow);
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(merge);
        self.builder.switch_to_block(merge);
        let new_value = self.builder.use_var(new_var);
        self.store_slot(slot, new_value);
        if push_result {
            if prefix {
                self.push(new_value);
            } else {
                self.push(old);
            }
        }
        self.fall_through(index);
        Ok(())
    }

    /// JS `ToBoolean` as a 0/1 I64 of FALSINESS: doubles inline (0, -0, NaN),
    /// the falsy/truthy tags inline, and any other heap value (an empty
    /// string is falsy per spec) through `to_boolean_slow`.
    fn emit_truthiness(&mut self, bits: ClifValue) -> Result<ClifValue, Unsupported> {
        let is_double = self.is_double(bits);
        let num = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), bits);
        let zero = self.builder.ins().f64const(0.0);
        let eq0 = self.builder.ins().fcmp(FloatCC::Equal, num, zero);
        let nan = self.builder.ins().fcmp(FloatCC::Unordered, num, num);
        let eq0_64 = self.bint(eq0);
        let nan_64 = self.bint(nan);
        let falsy_num = self.builder.ins().bor(eq0_64, nan_64);
        let f = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, bits, self.false_bits);
        let n = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, bits, self.null_bits);
        let u = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, bits, self.undef_bits);
        let f_64 = self.bint(f);
        let n_64 = self.bint(n);
        let u_64 = self.bint(u);
        let f_or_n = self.builder.ins().bor(f_64, n_64);
        let falsy_tag = self.builder.ins().bor(f_or_n, u_64);
        let true_cmp = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, bits, self.true_bits);
        let is_true = self.bint(true_cmp);
        let known = self.builder.ins().bor(falsy_tag, is_true);
        let fast_falsy = self.builder.ins().select(is_double, falsy_num, falsy_tag);
        // Slow path: a heap value that is not one of the known tags
        // (bigint/string/symbol/object/function — the empty string is falsy).
        let is_double64 = self.bint(is_double);
        let not_dbl_cmp = self.builder.ins().icmp_imm_u(IntCC::Equal, is_double64, 0);
        let not_double = self.bint(not_dbl_cmp);
        let unknown_cmp = self.builder.ins().icmp_imm_u(IntCC::Equal, known, 0);
        let unknown = self.bint(unknown_cmp);
        let needs_slow = self.builder.ins().band(not_double, unknown);
        let falsy_var = self.builder.declare_var(types::I64);
        self.builder.def_var(falsy_var, fast_falsy);
        let merge = self.builder.create_block();
        let slow = self.builder.create_block();
        self.builder.ins().brif(needs_slow, slow, &[], merge, &[]);
        self.builder.switch_to_block(slow);
        let truthy = self.call_slow(self.sig_bool, Helper::ToBooleanSlow, &[bits])?;
        let falsy_cmp = self.builder.ins().icmp_imm_u(IntCC::Equal, truthy, 0);
        let falsy_slow = self.bint(falsy_cmp);
        self.builder.def_var(falsy_var, falsy_slow);
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(merge);
        self.builder.switch_to_block(merge);
        Ok(self.builder.use_var(falsy_var))
    }

    /// `nullish` — a pure tag comparison (undefined or null), no helper.
    fn emit_nullish(&mut self, bits: ClifValue) -> ClifValue {
        let u = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, bits, self.undef_bits);
        let n = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, bits, self.null_bits);
        self.builder.ins().bor(u, n)
    }

    /// Throw the TDZ ReferenceError when `bits` is the uninitialized marker.
    fn emit_tdz_check(&mut self, bits: ClifValue) -> Result<(), Unsupported> {
        let is_uninit = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, bits, self.uninit_bits);
        let merge = self.builder.create_block();
        let slow = self.builder.create_block();
        self.builder.ins().brif(is_uninit, slow, &[], merge, &[]);
        self.builder.switch_to_block(slow);
        // The helper throws; the trap catches a (buggy) return.
        let _ = self.call_slow(self.sig_tdz, Helper::TdzError, &[])?;
        self.builder
            .ins()
            .trap(TrapCode::user(1).expect("user trap code 1"));
        self.builder.seal_block(merge);
        self.builder.switch_to_block(merge);
        Ok(())
    }

    /// `emit_binary`: the number-number inline for the arithmetic/comparison
    /// shapes, `binary_slow` for everything else (and for non-Number
    /// operands).
    fn emit_binary(
        &mut self,
        op: BinaryOp,
        lhs: ClifValue,
        rhs: ClifValue,
    ) -> Result<ClifValue, Unsupported> {
        let Some(inline) = inline_binary(op) else {
            let op_imm = self.builder.ins().iconst(types::I64, op as i64);
            return self.call_slow(self.sig_binary, Helper::BinarySlow, &[op_imm, lhs, rhs]);
        };
        let lhs_num = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), lhs);
        let rhs_num = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), rhs);
        let fast = match inline {
            InlineBin::Arith(arith) => {
                let res = match arith {
                    ArithOp::Add => self.builder.ins().fadd(lhs_num, rhs_num),
                    ArithOp::Sub => self.builder.ins().fsub(lhs_num, rhs_num),
                    ArithOp::Mul => self.builder.ins().fmul(lhs_num, rhs_num),
                    ArithOp::Div => self.builder.ins().fdiv(lhs_num, rhs_num),
                };
                let bits = self
                    .builder
                    .ins()
                    .bitcast(types::I64, MemFlagsData::new(), res);
                self.canon(bits)
            }
            InlineBin::Cmp(cc) => {
                let c = self.builder.ins().fcmp(cc, lhs_num, rhs_num);
                let t = self.builder.ins().iconst(types::I64, self.true_bits);
                let f = self.builder.ins().iconst(types::I64, self.false_bits);
                self.builder.ins().select(c, t, f)
            }
        };
        let lhs_dbl = self.is_double(lhs);
        let rhs_dbl = self.is_double(rhs);
        let both = self.builder.ins().band(lhs_dbl, rhs_dbl);
        let res_var = self.builder.declare_var(types::I64);
        self.builder.def_var(res_var, fast);
        let merge = self.builder.create_block();
        let slow = self.builder.create_block();
        self.builder.ins().brif(both, merge, &[], slow, &[]);
        self.builder.switch_to_block(slow);
        let op_imm = self.builder.ins().iconst(types::I64, op as i64);
        let slow_res = self.call_slow(self.sig_binary, Helper::BinarySlow, &[op_imm, lhs, rhs])?;
        self.builder.def_var(res_var, slow_res);
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(merge);
        self.builder.switch_to_block(merge);
        Ok(self.builder.use_var(res_var))
    }

    // ----- register body (LeafOp) lowering -----

    fn emit_leaf_op(&mut self, op: &LeafOp) -> Result<(), Unsupported> {
        match op {
            LeafOp::LoadReg { slot, tdz } => {
                let bits = self.load_slot(*slot);
                if *tdz {
                    self.emit_tdz_check(bits)?;
                }
                self.builder.def_var(self.acc_var, bits);
            }
            LeafOp::LoadContext { .. } | LeafOp::LoadPerIter { .. } => {
                return Err(Unsupported::Leaf("context/per-iteration read"));
            }
            LeafOp::LoadCounter => {
                let bits = self.counter_bits();
                self.builder.def_var(self.acc_var, bits);
            }
            LeafOp::LoadConst(value) => {
                let bits = self.const_value(value)?;
                self.builder.def_var(self.acc_var, bits);
            }
            LeafOp::BinReg { op, slot, tdz } => {
                let right = self.load_slot(*slot);
                if *tdz {
                    self.emit_tdz_check(right)?;
                }
                let left = self.builder.use_var(self.acc_var);
                let res = self.emit_binary(*op, left, right)?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::BinContext { .. } | LeafOp::BinPerIter { .. } | LeafOp::BinCtxReg { .. } => {
                return Err(Unsupported::Leaf("context binary"));
            }
            LeafOp::BinImm { op, imm } => {
                let left = self.builder.use_var(self.acc_var);
                let right = self.const_value(&Value::Number(*imm))?;
                let res = self.emit_binary(*op, left, right)?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::BinConst { op, value } => {
                let left = self.builder.use_var(self.acc_var);
                let right = self.const_value(value)?;
                let res = self.emit_binary(*op, left, right)?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::BinImmLocal { op, slot, tdz, imm } => {
                let left = self.load_slot(*slot);
                if *tdz {
                    self.emit_tdz_check(left)?;
                }
                let right = self.const_value(&Value::Number(*imm))?;
                let res = self.emit_binary(*op, left, right)?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::BinAccPop { op } => {
                let left = self.pop();
                let right = self.builder.use_var(self.acc_var);
                let res = self.emit_binary(*op, left, right)?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::BinLeftReg { op, slot } => {
                let left = self.load_slot(*slot);
                let right = self.builder.use_var(self.acc_var);
                let res = self.emit_binary(*op, left, right)?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::StoreReg { slot, tdz } => {
                if *tdz {
                    let current = self.load_slot(*slot);
                    self.emit_tdz_check(current)?;
                }
                let value = self.builder.use_var(self.acc_var);
                self.store_slot(*slot, value);
            }
            LeafOp::StoreMemberName { name, value } => {
                let object = self.builder.use_var(self.acc_var);
                let value = self.leaf_operand(value)?;
                let name = self.builder.ins().iconst(types::I64, *name as i64);
                self.call_slow(
                    self.sig_set_name,
                    Helper::SetMemberName,
                    &[object, name, value],
                )?;
            }
            LeafOp::StoreMemberComputed { key, value } => {
                let object = self.builder.use_var(self.acc_var);
                let key = self.leaf_operand(key)?;
                let value = self.leaf_operand(value)?;
                self.call_slow(
                    self.sig_set_comp,
                    Helper::SetMemberComputed,
                    &[object, key, value],
                )?;
            }
            LeafOp::GetMemberName { name } => {
                let object = self.builder.use_var(self.acc_var);
                let name = self.builder.ins().iconst(types::I64, *name as i64);
                let res =
                    self.call_slow(self.sig_get_name, Helper::GetMemberName, &[object, name])?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::GetMemberComputed { key } => {
                let object = self.builder.use_var(self.acc_var);
                let key = self.leaf_operand(key)?;
                let res =
                    self.call_slow(self.sig_get_comp, Helper::GetMemberComputed, &[object, key])?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::GetMemberNameLocal {
                object_slot,
                tdz,
                name,
            } => {
                let object = self.load_slot(*object_slot);
                if *tdz {
                    self.emit_tdz_check(object)?;
                }
                let name = self.builder.ins().iconst(types::I64, *name as i64);
                let res =
                    self.call_slow(self.sig_get_name, Helper::GetMemberName, &[object, name])?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::GetMemberComputedLocal {
                object_slot,
                tdz,
                key,
            } => {
                let object = self.load_slot(*object_slot);
                if *tdz {
                    self.emit_tdz_check(object)?;
                }
                let key = self.leaf_operand(key)?;
                let res =
                    self.call_slow(self.sig_get_comp, Helper::GetMemberComputed, &[object, key])?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::PushAcc => {
                let bits = self.counter_bits();
                self.push(bits);
            }
            LeafOp::ReturnAcc => {
                let value = self.builder.use_var(self.acc_var);
                self.builder.ins().return_(&[value]);
            }
        }
        Ok(())
    }

    /// Load a `RegOperand` (a member op's key/value): a frame slot (with its
    /// `tdz` check), a constant, or the loop counter.
    fn leaf_operand(&mut self, operand: &RegOperand) -> Result<ClifValue, Unsupported> {
        match operand {
            RegOperand::Reg { slot, tdz } => {
                let bits = self.load_slot(*slot);
                if *tdz {
                    self.emit_tdz_check(bits)?;
                }
                Ok(bits)
            }
            RegOperand::Const(value) => self.const_value(value),
            RegOperand::Counter => Ok(self.counter_bits()),
            RegOperand::Acc
            | RegOperand::Spilled
            | RegOperand::Ctx { .. }
            | RegOperand::PerIter { .. } => Err(Unsupported::Leaf("member operand shape")),
        }
    }
}
