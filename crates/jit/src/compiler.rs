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
use runtime::ir::{
    ApplyKind, CompiledBody, FastLoopVar, GLOBAL_CELLS, LeafOp, MEMBER_CELLS, MemberValueCell,
    RegOperand, RelLimit, ScopeInfo, Step, is_compound_assign,
};
use runtime::jit::{
    AGENT_REALM_COUNT_OFFSET, GlobalValueCell, JIT_APPLY_MAX_ARGS, JitCallContext,
    LEAF_CALL_CACHE_ENTRIES, LeafCallSiteCache, LeafInlineInfo, TYPED_ARRAY_LENGTH_SENTINEL,
    VM_ASYNC_FOR_OF_STACK_LEN_OFFSET, VM_COMPLETION_IS_EMPTY_OFFSET, VM_COMPLETION_OFFSET,
    VM_DESTRUCTURE_STACK_LEN_OFFSET, VM_ENV_STACK_LEN_OFFSET, VM_FOR_IN_STACK_LEN_OFFSET,
    VM_FOR_OF_BOUNDARIES_LEN_OFFSET, VM_FOR_OF_STACK_LEN_OFFSET, VM_PENDING_LEN_OFFSET,
    VM_TRY_STACK_LEN_OFFSET,
};
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
        let result = lower(body, helpers, &mut func, &mut fctx, &*self.isa, conv);
        result.ok()?;
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
        let info = crate::JitCompiledInfo {
            entry: entry as usize,
            stack_usage: max_stack_usage(body),
        };
        Some(Compiled { code, info })
    }
}

/// The body's maximum value-stack depth above the entry stack pointer, in
/// slots — the JIT's working-area size the Vm integration must pre-allocate.
/// A conservative straight-line bound: the compiler emits balanced push/pop
/// pairs (the interpreter's own stack discipline keeps every loop body
/// balanced, so the first-pass depth at a loop head is the depth each
/// iteration sees), and a `RunRegBody` truncates its transient use back to
/// the entry depth. Unsupported steps bail out of `compile` before this is
/// consulted, so the un-modeled variants need no entry here.
fn max_stack_usage(body: &CompiledBody) -> usize {
    let mut depth = 0usize;
    let mut max = 0usize;
    for step in &body.steps {
        match step {
            Step::Push(_) | Step::Dup | Step::PushAcc => depth += 1,
            Step::Pop => depth = depth.saturating_sub(1),
            // Two operands in, one result out.
            Step::Binary(_) | Step::BinaryImm { .. } => depth = depth.saturating_sub(1),
            // The test pops; the keep variants leave the value.
            Step::JumpIfFalse(_) | Step::JumpIfTrue(_) => depth = depth.saturating_sub(1),
            // The hoist guard pops the receiver and pushes the length back on
            // a probe hit (net 0); the miss jumps to a fresh evaluation.
            Step::TypedArrayLengthHoist { .. } => {}
            // A store consumes the value; `UpdateLocal` pushes its result.
            Step::StoreLocal { .. } | Step::FusedStoreLocal { .. } | Step::InitLocal { .. } => {
                depth = depth.saturating_sub(1)
            }
            Step::UpdateLocal { .. } => depth += 1,
            // `SetCompletion` pops the statement's value (the interpreter's
            // handler does; a JIT no-op would accumulate one slot per
            // statement inside a loop).
            Step::SetCompletion => depth = depth.saturating_sub(1),
            // Capture-context steps: a read pushes; a write pops; the fused
            // update pushes its result. Per-iteration steps mirror them.
            Step::LoadContextSlot { .. }
            | Step::UpdateContextSlot { .. }
            | Step::LoadPerIteration { .. }
            | Step::UpdatePerIteration { .. } => depth += 1,
            Step::StoreContextSlot { .. }
            | Step::InitContextSlot { .. }
            | Step::StorePerIteration { .. } => depth = depth.saturating_sub(1),
            // Member read: pop, push the result back. Member writes pop the
            // object + value (and the cached old value for a compound op) and
            // push the stored value back.
            Step::GetMemberName { .. } => {}
            Step::GetMemberComputed => depth = depth.saturating_sub(1),
            Step::AssignMemberName { op, .. } => {
                let popped = if is_compound_assign(op) { 3 } else { 2 };
                depth = depth.saturating_sub(popped).saturating_add(1);
            }
            Step::AssignMemberComputed { op } => {
                let popped = if is_compound_assign(op) { 4 } else { 3 };
                depth = depth.saturating_sub(popped).saturating_add(1);
            }
            // Super property access (Cut 61): `GetSuperBase`/`ThisValue`
            // push the base/this; the reads pop the base(+key) and push the
            // value back (net 0 / -1); the Keep pops the duplicated pair +
            // write-copy key, advances past the converted key, and pushes the
            // value (net -1); the assigns mirror the member forms; the
            // updates pop old(+key)+base and push the result; the computed
            // reference resolve consumes base+key into the reference stack;
            // the name form and `DeleteSuper` leave the work stack as-is.
            Step::GetSuperBase | Step::ThisValue => depth += 1,
            Step::GetSuperName { .. } => {}
            Step::GetSuperComputed | Step::GetSuperComputedKeep => depth = depth.saturating_sub(1),
            Step::AssignSuperName { op, .. } => {
                let popped = if is_compound_assign(op) { 3 } else { 2 };
                depth = depth.saturating_sub(popped).saturating_add(1);
            }
            Step::AssignSuperComputed { op } => {
                let popped = if is_compound_assign(op) { 4 } else { 3 };
                depth = depth.saturating_sub(popped).saturating_add(1);
            }
            Step::UpdateSuperName { .. } => depth = depth.saturating_sub(1),
            Step::UpdateSuperComputed { .. } => depth = depth.saturating_sub(2),
            Step::DeleteSuper | Step::ResolveSuperRefName { .. } => {}
            Step::ResolveSuperRefComputed => depth = depth.saturating_sub(2),
            // The register body's transient pushes (each `PushAcc`) are
            // unwound by the entry-depth truncate.
            Step::RunRegBody { ops } => {
                let transient = ops
                    .iter()
                    .filter(|op| matches!(op, LeafOp::PushAcc))
                    .count();
                max = max.max(depth + transient);
            }
            // The call pops `this` + callee + `argc` args and pushes the
            // result.
            Step::CallFast { argc, .. } => {
                depth = depth.saturating_sub(*argc as usize + 2).saturating_add(1);
            }
            // The compiled apply/call step pops the same region and pushes
            // the result. M10: the intrinsic fast path additionally copies a
            // dense `argArray`'s elements (up to `JIT_APPLY_MAX_ARGS` slots)
            // above the current stack top before the in-frame leaf run —
            // reserve the room or the copy always bails to the slow path
            // (the leaf's own frame/work fit in the slack beyond the
            // reserve, and the probe rejects a larger leaf).
            Step::CallApply { argc, .. } => {
                max = max.max(depth + JIT_APPLY_MAX_ARGS);
                depth = depth.saturating_sub(*argc as usize + 2).saturating_add(1);
            }
            // The vector form (Cut 49): `ArgsPush`/`ArgsSpread` consume the
            // value (it goes into the Vm's argument vector, not the work
            // stack); the vector `Call` pops `this` + callee and pushes the
            // result; the vector `TailCall` pops both and terminates.
            Step::ArgsPush | Step::ArgsSpread => depth = depth.saturating_sub(1),
            Step::Call { .. } => depth = depth.saturating_sub(1),
            Step::TailCall { .. } => depth = depth.saturating_sub(2),
            // Array literals (Cut 52): `ArrayBegin` pushes the array;
            // `ArrayElement`/`ArraySpread` pop the element(s) + the array and
            // push the array back; `ArrayHole`/`ArrayEnd` keep the array on
            // the stack.
            Step::ArrayBegin => depth += 1,
            Step::ArrayElement | Step::ArraySpread => {
                depth = depth.saturating_sub(2).saturating_add(1)
            }
            Step::ArrayHole | Step::ArrayEnd => {}
            // Object literals (Cut 53): `ObjectBegin` pushes the object; an
            // `Init` pops its value(s) + the object and pushes it back; the
            // method/accessor/key/spread steps keep it net-neutral or pop
            // the property's value.
            Step::ObjectBegin => depth += 1,
            Step::ObjectInitName { .. }
            | Step::ObjectMethodComputed { .. }
            | Step::ObjectAccessorComputed { .. }
            | Step::ObjectSpread => depth = depth.saturating_sub(2).saturating_add(1),
            Step::ObjectInitComputed { .. } => depth = depth.saturating_sub(3).saturating_add(1),
            Step::ObjectKeyToPropertyKey
            | Step::ObjectMethodName { .. }
            | Step::ObjectAccessorName { .. } => {}
            // String literals (Cut 54): `PushStr` pushes the literal;
            // `ConcatStr` pops the value + accumulator and pushes the
            // concatenation; `ConcatStrConst` swaps the accumulator for the
            // concatenation.
            Step::PushStr(_) => depth += 1,
            Step::ConcatStr => depth = depth.saturating_sub(2).saturating_add(1),
            Step::ConcatStrConst(_) => {}
            // The fused slot call pops `argc` args (the callee is the
            // frame slot) and pushes the result.
            Step::CallFastSlot { argc, .. } | Step::CallFastGlobal { argc, .. } => {
                depth = depth.saturating_sub(*argc as usize).saturating_add(1);
            }
            // The fused `x = f(args)` stores (Cut 65): the materialized arg
            // slots transiently raise the depth; the call replaces them with
            // the result and the store pops it (net 0).
            Step::CallFastSlotStore { arg_slots, .. }
            | Step::CallFastGlobalStore { arg_slots, .. } => {
                depth += arg_slots.len();
                depth = depth
                    .saturating_sub(arg_slots.len())
                    .saturating_add(1)
                    .saturating_sub(1);
            }
            // A global read pushes the value; a global write consumes it.
            Step::LoadGlobal { .. } => depth += 1,
            Step::StoreGlobal { .. } | Step::FusedStoreGlobal { .. } => {
                depth = depth.saturating_sub(1)
            }
            // The identifier read pushes; a reference write pops and
            // re-pushes (net 0); the identifier update pops and re-pushes.
            Step::LoadIdent { .. } => depth += 1,
            Step::ResolveVarIdent { .. } => {}
            Step::PutVarReference => {}
            Step::UpdateIdent { .. } => {}
            // The reference machinery: `GetVarReference` pushes the value;
            // the update pops the old and pushes the result (net 0); the
            // compound pops both and pushes the result (net -1).
            Step::GetVarReference => depth += 1,
            Step::UpdateVarReference { .. } => {}
            Step::PutVarReferenceOp { .. } => depth = depth.saturating_sub(1),
            Step::PopVarReference => {}
            // Control steps (Cut 55): `Return`/`Throw` pop their value; the
            // transfers (`Exit`/`Break`/`Continue`/`FinallyEnd`) and the
            // try/block machinery leave the stack as-is.
            Step::Return | Step::Throw => depth = depth.saturating_sub(1),
            // Switch steps (Cut 56): `SwitchDisc` and `SwitchTest` pop.
            Step::SwitchDisc | Step::SwitchTest { .. } => depth = depth.saturating_sub(1),
            // for-in/for-of machinery (Cut 57): `ForInBegin`/`ForOfBegin`
            // pop the RHS; the fetch steps push the element (the `done`
            // path pushes nothing — the worst case bounds the working
            // area); `ForOfNextBindLocal` lands it in a frame slot; the
            // bind steps pop it; the env-creation steps and `ForOfClose`
            // leave the value stack as-is.
            Step::ForInBegin | Step::ForOfBegin { .. } => depth = depth.saturating_sub(1),
            Step::ForInNext { .. } | Step::ForOfNext { .. } => depth += 1,
            Step::ForOfNextBindLocal { .. } => {}
            Step::ForOfBindLocal { .. } | Step::ForOfBindGlobal { .. } => {
                depth = depth.saturating_sub(1)
            }
            Step::ForOfClose | Step::EnterPerIteration { .. } | Step::PerIteration { .. } => {}
            // Suspension (Cut 58): `Yield`/`Await` pop the value into the
            // suspension payload (the saved region is what's below).
            Step::Yield { .. } | Step::Await => depth = depth.saturating_sub(1),
            // Destructuring (Cut 59): `DestructureBegin`/`DestructureObjCoercible`
            // pop the value onto the Vm's pattern stacks; `DestructureNext`/
            // `DestructureRest`/`DestructureObjKey`/`DestructureObjKeyGet`/
            // `DestructureObjRest` push the element/key/rest value;
            // `DestructureObjKeyComputed` swaps key for value; the close/end
            // steps are net-neutral.
            Step::DestructureBegin => depth = depth.saturating_sub(1),
            Step::DestructureNext => depth += 1,
            Step::DestructureUndef { .. } => {}
            Step::DestructureRest => depth += 1,
            Step::DestructureObjCoercible => depth = depth.saturating_sub(1),
            Step::DestructureObjKey { .. } => depth += 1,
            Step::DestructureObjKeyComputed => {}
            Step::DestructureObjKeyStore => depth = depth.saturating_sub(1),
            Step::DestructureObjKeyGet => depth += 1,
            Step::DestructureObjRest { .. } => depth += 1,
            Step::DestructureClose | Step::DestructureObjEnd => {}
            _ => {}
        }
        max = max.max(depth);
    }
    max
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

/// The byte offset of a `LeafInlineInfo` field inside `JitCallContext`'s
/// `leaf_call_cache` record (Cranelift has no `offset_of!`, so the compiled
/// code loads the fields at these fixed offsets).
fn leaf_inline_offset(field: usize) -> usize {
    std::mem::offset_of!(LeafCallSiteCache, leaf_inline) + field
}

/// The byte offset of the `leaf_call_cache` record for call-site step `index`
/// inside `JitCallContext` (Cut 68: the direct-mapped set — the step index is
/// a compile-time constant per call site, so the slot is a baked immediate).
fn leaf_cache_entry_offset(index: usize) -> usize {
    std::mem::offset_of!(JitCallContext, leaf_call_cache)
        + (index ^ (index >> 2)) % LEAF_CALL_CACHE_ENTRIES
            * std::mem::size_of::<LeafCallSiteCache>()
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
        Step::CallApply { .. } => "CallApply",
        Step::CallFastGlobal { .. }
        | Step::CallFastSlot { .. }
        | Step::CallFastGlobalStore { .. }
        | Step::CallFastSlotStore { .. } => "CallFastGlobal/Slot",
        Step::ArgsBase | Step::ArgsPush | Step::ArgsSpread => "ArgsVector",
        Step::TailCall { .. }
        | Step::TailCallFast { .. }
        | Step::TailCallFastGlobal { .. }
        | Step::TailCallFastSlot { .. }
        | Step::TailCallSelf { .. }
        | Step::TailCallSelfCheck { .. }
        | Step::TailCallSelfVector
        | Step::TailCallSelfCheckVector => "TailCall",
        Step::Throw => "Throw",
        Step::LoadIdent { .. } => "LoadIdent",
        Step::Unary(_) => "Unary",
        Step::EnterTry { .. } => "EnterTry",
        Step::EnterWith => "EnterWith",
        Step::CreateFunction { .. } | Step::CreateArrow { .. } => "CreateFunction",
        Step::LoadGlobal { .. } | Step::StoreGlobal { .. } | Step::UpdateGlobal { .. } => {
            "Global fast path"
        }
        Step::ArrayBegin
        | Step::ArrayElement
        | Step::ArraySpread
        | Step::ArrayHole
        | Step::ArrayEnd => "ArrayLiteral",
        Step::ObjectBegin
        | Step::ObjectInitName { .. }
        | Step::ObjectInitComputed { .. }
        | Step::ObjectKeyToPropertyKey
        | Step::ObjectMethodName { .. }
        | Step::ObjectMethodComputed { .. }
        | Step::ObjectAccessorName { .. }
        | Step::ObjectAccessorComputed { .. }
        | Step::ObjectSpread => "ObjectLiteral",
        Step::PushStr(_) | Step::ConcatStr | Step::ConcatStrConst(_) => "StringLiteral",
        Step::ForOfBind { .. }
        | Step::ForInBind { .. }
        | Step::ForOfRestore
        | Step::ForInRestore => "ForOf/ForIn env heads",
        Step::AsyncForOfBegin { .. }
        | Step::AsyncForOfNext
        | Step::AsyncForOfTest { .. }
        | Step::AsyncForOfBind { .. }
        | Step::AsyncForOfRestore
        | Step::AsyncForOfClose => "AsyncForOf",
        Step::EnterPerIteration { .. } | Step::PerIteration { .. } => "Per-iteration env",
        Step::SwitchDisc | Step::SwitchTest { .. } => "Switch",
        Step::Break { .. } | Step::Continue { .. } => "Break/Continue",
        Step::YieldStarBegin
        | Step::YieldStarNext { .. }
        | Step::YieldStarResume { .. }
        | Step::AsyncYieldStarBegin
        | Step::AsyncYieldStarNext { .. }
        | Step::AsyncYieldStarInspect { .. }
        | Step::AsyncYieldStarResume { .. } => "YieldStar/AsyncYieldStar",
        Step::Destructure { .. } | Step::DeclInit { .. } => "Destructure",
        Step::CreateArguments { .. } => "CreateArguments",
        Step::TypeofTop => "TypeofTop",
        Step::ImportCall { .. } | Step::ImportMeta => "Import",
        Step::ResolveVarIdent { .. } | Step::GetVarReferenceThis => "Reference machinery",
        Step::GetSuperName { .. }
        | Step::GetSuperComputed
        | Step::GetSuperComputedKeep
        | Step::GetSuperBase
        | Step::ThisValue
        | Step::AssignSuperName { .. }
        | Step::AssignSuperComputed { .. }
        | Step::UpdateSuperName { .. }
        | Step::UpdateSuperComputed { .. }
        | Step::DeleteSuper
        | Step::ResolveSuperRefName { .. }
        | Step::ResolveSuperRefComputed => "Super",
        Step::LoadContextSlot { .. } | Step::StoreContextSlot { .. } => "Context slot",
        Step::InitContextSlot { .. } | Step::UpdateContextSlot { .. } => "Context slot",
        Step::SetCompletion
        | Step::ResetCompletion
        | Step::NormalizeCompletion
        | Step::ListBegin
        | Step::ListEnd
        | Step::SaveCompletion
        | Step::RestoreCompletion => "Completion",
        Step::JumpIfRelLimit { .. } => "JumpIfRelLimit",
        Step::TypedArrayLengthHoist { .. } => "TypedArrayLengthHoist",
        Step::LoadPerIteration { .. }
        | Step::StorePerIteration { .. }
        | Step::UpdatePerIteration { .. } => "Per-iteration",
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
        | Step::JumpIfGeGlobalImm { target, .. }
        | Step::JumpIfRelLimit { target, .. } => vec![*target],
        Step::TypedArrayLengthHoist { target } => vec![*target],
        Step::FastLoopHead {
            body_start, after, ..
        } => vec![*body_start, *after],
        Step::JumpIfChainShort(t) => vec![*t],
        Step::Exit { after } => vec![*after],
        Step::SwitchTest { case } => vec![*case],
        Step::ForInNext { back, .. } | Step::ForOfNext { back, .. } => vec![*back],
        Step::ForOfNextBindLocal { back, .. } => vec![*back],
        // Cut 59: a `DestructureUndef` default's jump target (always a
        // forward label within the pattern, but a static branch either way).
        Step::DestructureUndef { use_default } => vec![*use_default],
        // Cut 46: a self-tail-call jumps to the body's re-entry block, not
        // to a step block (see `Lowerer::reentry_block`).
        Step::TailCallSelf { .. }
        | Step::TailCallSelfCheck { .. }
        | Step::TailCallSelfVector
        | Step::TailCallSelfCheckVector => Vec::new(),
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
    /// Cut 70: one variable per handler, holding the working-sp at the
    /// handler's `EnterTry` — a covered error (or a finally route) resumes
    /// the catch/finally there, discarding the erroring step's dead
    /// operands (see `emit_step`'s entry reset and the `EnterTry` save).
    handler_sp_vars: Vec<Variable>,
    /// Cut 70: handler-entry step index (a `catch.start` or `finally`
    /// start) → handler index — the dispatch-only blocks whose code must
    /// reset the working-sp to the saved try-entry sp first.
    handler_entry_steps: std::collections::HashMap<usize, usize>,
    /// Cut 46: the working-stack base the body started with — the `sp_var`
    /// value at entry, saved so a self-tail-call back edge can reset the
    /// working stack to the fresh-run base before re-entering the body.
    entry_sp_var: Variable,
    /// Cut 46: the re-entry block a `TailCallSelf` back edge jumps to — it
    /// re-seeds the per-run variables (working-stack base, loop counter,
    /// accumulator) and enters step 0. `Some` only when the body contains a
    /// `TailCallSelf` (cranelift forbids jumping to the function's entry
    /// block, so a body with a self-tail-call routes its entry through this
    /// block instead).
    reentry_block: Option<Block>,
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
    sig_rmw: SigRef,
    sig_call: SigRef,
    /// The `(vm, callee, this, argc, args_ptr, direct_eval) -> value`
    /// signature of `call_slow` (Cut 62: a direct-eval `CallFast` site
    /// passes the flag through; the compiler never emits one — direct eval
    /// always takes the vector form — but the step is then fully lowered).
    sig_call_slow: SigRef,
    /// The `(vm, resolved, callee, argc, args_ptr, kind) -> value` signature
    /// of `call_apply` (the compiled `Step::CallApply`).
    sig_call_apply: SigRef,
    /// M10: the `(vm, arg_array, dest) -> count` signature of the compiled
    /// `CallApply` fast path's dense-element fill (`apply_args_fill`).
    sig_apply_args: SigRef,
    sig_assign: SigRef,
    /// The `(vm, step_index) -> value` signature: the closure/RegExp helpers
    /// read their step's payload back out of the running body instead of
    /// marshalling it across the FFI boundary.
    sig_step: SigRef,
    /// Cut 55: whether the body contains try machinery (`EnterTry`). A
    /// try body's `Return` routes through `return_control`, its helpers'
    /// pending errors dispatch through `dispatch_error`, and
    /// `SaveCompletion`/`RestoreCompletion` are no-ops (the pending control
    /// carries the values; the completion register is unobservable).
    has_try: bool,
    /// Cut 57: whether the body contains for-of machinery (`ForOfBegin`/the
    /// fetch steps). A for-of body's `Return` must close the active iterator
    /// (route through `return_control` — the interpreter's `control_transfer`
    /// closes on the escape), and a helper's engine error must close the
    /// iterators before the body returns (the callee Vm's stacks are
    /// discarded on the error surface).
    has_for_of: bool,
    /// Cut 59: whether the body contains destructuring machinery (any
    /// `Destructure*` step). A helper's engine error in such a body closes
    /// the active destructure iterators first (mirroring `run_inner`'s
    /// uncovered-error close, skipping a `next()` error) and `dispatch_error`
    /// does the same regardless of coverage.
    has_destructure: bool,
    /// Cut 58: whether the body contains a suspension step (`Yield`/
    /// `Await`). A suspension body gets an entry-forwarder block that
    /// dispatches the resume: a normal resume jumps to the continuation
    /// block, a throw/return resume routes through the control machinery.
    has_suspension: bool,
    /// Cut 58: the step indices a normal resume can enter at — the step
    /// after each `Yield`/`Await` (the interpreter's `vm.ip` at the
    /// suspension). The entry's resume chain compares `ctx.resume_ip`
    /// against these.
    suspension_targets: Vec<usize>,
    /// Cut 55: the static set of step indexes a control-transfer dispatch
    /// can jump to — every `Exit`'s `after`, every `Break`/`Continue`
    /// target, and every handler's catch/finally start. The dispatch
    /// helpers return one of these (or a completion sentinel), and the
    /// machine code branches over this set.
    dispatch_targets: Vec<usize>,
    /// Cut 55: the step being lowered — the pending-error dispatch passes
    /// `current_step + 1` as the interpreter ip (the loop-top increment).
    current_step: usize,
    /// Cut 55: when a register body (`RunRegBody`) is being lowered, the
    /// working-stack pointer at its entry — a helper error inside the run
    /// must truncate the transient stack use back to it BEFORE dispatching
    /// (the interpreter truncates before propagating; a catch block reads
    /// the sp at the `RunRegBody` step).
    error_sp: Option<ClifValue>,
    // NaN-boxing bit patterns (see `crux::value`).
    /// The `(vm, callee, this, argc, args, direct_eval) -> value` signature
    /// of the tail-call helper (Cut 45).
    sig_tail: SigRef,
    /// The JIT entry signature `(frame, stack, vm) -> value` — the in-frame
    /// leaf-call path calls the callee's compiled entry with it.
    sig_entry: SigRef,
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
        // Cut 70: per-handler try-entry sp snapshots (see the struct field).
        let handler_sp_vars: Vec<Variable> = (0..body.handlers.len())
            .map(|_| builder.declare_var(types::I64))
            .collect();
        let mut handler_entry_steps = std::collections::HashMap::new();
        for (index, handler) in body.handlers.iter().enumerate() {
            if let Some(catch) = handler.catch {
                handler_entry_steps.insert(catch.start, index);
            }
            if let Some(finally) = handler.finally {
                handler_entry_steps.insert(finally, index);
            }
        }
        let entry_sp_var = builder.declare_var(types::I64);
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
        let sig_rmw = builder.import_signature(helper_sig(&[types::I64; 5], conv));
        let sig_call = builder.import_signature(helper_sig(&[types::I64; 5], conv));
        let sig_call_slow = builder.import_signature(helper_sig(&[types::I64; 6], conv));
        let sig_call_apply = builder.import_signature(helper_sig(&[types::I64; 6], conv));
        let sig_apply_args = builder.import_signature(helper_sig(&[types::I64; 3], conv));
        let sig_assign = builder.import_signature(helper_sig(&[types::I64; 6], conv));
        let sig_step = builder.import_signature(helper_sig(&[types::I64; 2], conv));
        let sig_tail = builder.import_signature(helper_sig(&[types::I64; 6], conv));
        let sig_entry = builder.import_signature(helper_sig(&[types::I64; 3], conv));
        // Cut 55: the body's try machinery and its static dispatch targets
        // (see `dispatch_targets`).
        let has_try = body
            .steps
            .iter()
            .any(|step| matches!(step, Step::EnterTry { .. }));
        let has_for_of = body.steps.iter().any(|step| {
            matches!(
                step,
                Step::ForOfBegin { .. } | Step::ForOfNext { .. } | Step::ForOfNextBindLocal { .. }
            )
        });
        // Cut 59: the body's destructuring machinery (any primitive
        // `Destructure*` step) — the error path must close the active
        // destructure iterators before surfacing a pending engine error.
        let has_destructure = body.steps.iter().any(|step| {
            matches!(
                step,
                Step::DestructureBegin
                    | Step::DestructureNext
                    | Step::DestructureUndef { .. }
                    | Step::DestructureRest
                    | Step::DestructureObjCoercible
                    | Step::DestructureObjKey { .. }
                    | Step::DestructureObjKeyComputed
                    | Step::DestructureObjKeyStore
                    | Step::DestructureObjKeyGet
                    | Step::DestructureObjRest { .. }
                    | Step::DestructureClose
                    | Step::DestructureObjEnd
            )
        });
        let has_suspension = body
            .steps
            .iter()
            .any(|step| matches!(step, Step::Yield { .. } | Step::Await));
        let mut suspension_targets = Vec::new();
        for (index, step) in body.steps.iter().enumerate() {
            if matches!(step, Step::Yield { .. } | Step::Await) {
                // The continuation is the next step (possibly the
                // past-the-end block when the suspension is last).
                suspension_targets.push(index + 1);
            }
        }
        let mut dispatch_targets = std::collections::BTreeSet::new();
        for step in &body.steps {
            match step {
                Step::Exit { after }
                | Step::Break { target: after }
                | Step::Continue { target: after } => {
                    dispatch_targets.insert(*after);
                }
                _ => {}
            }
        }
        for handler in &body.handlers {
            if let Some(catch) = handler.catch {
                dispatch_targets.insert(catch.start);
            }
            if let Some(finally) = handler.finally {
                dispatch_targets.insert(finally);
            }
        }
        let dispatch_targets: Vec<usize> = dispatch_targets.into_iter().collect();
        Lowerer {
            builder,
            helpers,
            scope: body.scope.as_ref(),
            blocks: Vec::new(),
            back_targets: HashSet::new(),
            reentry_block: None,
            frame_var,
            sp_var,
            handler_sp_vars,
            handler_entry_steps,
            entry_sp_var,
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
            sig_rmw,
            sig_call,
            sig_call_slow,
            sig_call_apply,
            sig_apply_args,
            sig_assign,
            sig_step,
            sig_tail,
            sig_entry,
            has_try,
            has_for_of,
            has_destructure,
            has_suspension,
            suspension_targets,
            dispatch_targets,
            current_step: 0,
            error_sp: None,
            undef_bits: Value::Undefined.bits() as i64,
            null_bits: Value::Null.bits() as i64,
            false_bits: Value::Boolean(false).bits() as i64,
            true_bits: Value::Boolean(true).bits() as i64,
            uninit_bits: Value::uninitialized().bits() as i64,
            canon_nan_bits: Value::Number(f64::NAN).bits() as i64,
        }
    }

    // ----- blocks and the step walk -----

    /// Get or create the block for a step index. Step blocks carry no
    /// parameters — the function's own entry block (the first created block,
    /// or a dedicated forwarder when the body has a self-tail-call) binds the
    /// frame/stack/vm parameters, and every other block's variables flow in
    /// through the SSA machinery.
    fn ensure_block(&mut self, index: usize) -> Block {
        if let Some(block) = self.blocks[index] {
            return block;
        }
        let block = self.builder.create_block();
        if index == 0 {
            // The step-0 block is the function entry on the common path (its
            // parameters ARE the function's). A self-tail-call body creates
            // its own entry block before step 0 instead (see `emit_all`).
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
            self.builder.def_var(self.entry_sp_var, params[1]);
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
        // Cut 55: the control-transfer dispatch can jump to ANY dispatch
        // target at runtime — including a forward one (a step's own
        // pending-error dispatch targets its own catch start, and the
        // catch/finally blocks are reached only through the dispatches).
        // Their blocks must stay unsealed until `seal_all_blocks`, or a
        // later dispatch branch hits the sealed-block assertion.
        for target in &self.dispatch_targets {
            self.back_targets.insert(*target);
        }
        // Cut 46: a body with a self-tail-call needs a re-entry block the
        // back edge can jump to — cranelift forbids jumping to the
        // function's ENTRY block, so the entry binds the parameters and
        // falls into a re-entry block that seeds the per-run variables
        // (the working-stack base, the loop counter, the accumulator) and
        // enters step 0. A fresh body run through either path starts from
        // exactly the entry state.
        let has_self_tail_call = body.steps.iter().any(|step| {
            matches!(
                step,
                Step::TailCallSelf { .. }
                    | Step::TailCallSelfCheck { .. }
                    | Step::TailCallSelfVector
                    | Step::TailCallSelfCheckVector
            )
        });
        if has_self_tail_call || self.has_suspension {
            // The function's ENTRY block is the first block to receive an
            // instruction; it binds the parameters and falls into the
            // re-entry block (a self-tail-call back edge) or the resume
            // dispatch (a suspension). (Cranelift forbids jumping to the
            // entry block, so the self-tail-call back edge targets the
            // re-entry block instead.)
            let entry = self.builder.create_block();
            self.builder.append_block_params_for_function_params(entry);
            self.builder.switch_to_block(entry);
            let params = self.builder.block_params(entry).to_vec();
            self.builder.def_var(self.frame_var, params[0]);
            self.builder.def_var(self.entry_sp_var, params[1]);
            self.builder.def_var(self.vm_var, params[2]);
            let step0 = self.builder.create_block();
            self.blocks[0] = Some(step0);
            if self.has_suspension {
                // The resume dispatch (Cut 58): the ctx's `resume_kind`
                // picks the path — 0 (normal) jumps to the continuation
                // block (`resume_ip`, compared against the body's static
                // suspension targets; 0 = a fresh run); 1/2 (throw/return)
                // route through the control machinery with the resume value
                // (the `dispatch_targets` compare chain) — mirroring
                // `vm.run`/`vm.run_abrupt` for a resumed generator/async
                // body.
                let vm = self.vm();
                let kind = self.builder.ins().load(
                    types::I8,
                    MemFlagsData::new(),
                    vm,
                    Offset32::new(std::mem::offset_of!(JitCallContext, resume_kind) as i32),
                );
                let kind64 = self.builder.ins().uextend(types::I64, kind);
                let normal_block = self.builder.create_block();
                let mach_block = self.builder.create_block();
                let is_normal = self.builder.ins().icmp_imm_u(IntCC::Equal, kind64, 0);
                self.builder
                    .ins()
                    .brif(is_normal, normal_block, &[], mach_block, &[]);
                // The machinery paths: restore the sp, then run the
                // throw/return transfer with the resume value.
                let throw_block = self.builder.create_block();
                let return_block = self.builder.create_block();
                self.builder.switch_to_block(mach_block);
                let is_throw = self.builder.ins().icmp_imm_u(IntCC::Equal, kind64, 1);
                self.builder
                    .ins()
                    .brif(is_throw, throw_block, &[], return_block, &[]);
                self.builder.switch_to_block(throw_block);
                self.emit_resume_abrupt(Helper::ThrowControl)?;
                self.builder.switch_to_block(return_block);
                self.emit_resume_abrupt(Helper::ReturnControl)?;
                // The normal path: a fresh run (resume_ip 0) seeds the
                // per-run variables from the stack parameter and enters step
                // 0; a resume uses the restored working region and jumps to
                // the continuation block.
                let fresh_block = self.builder.create_block();
                let resume_block = self.builder.create_block();
                self.builder.switch_to_block(normal_block);
                let resume_ip = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    vm,
                    Offset32::new(std::mem::offset_of!(JitCallContext, resume_ip) as i32),
                );
                let is_fresh = self.builder.ins().icmp_imm_u(IntCC::Equal, resume_ip, 0);
                self.builder
                    .ins()
                    .brif(is_fresh, fresh_block, &[], resume_block, &[]);
                self.builder.switch_to_block(fresh_block);
                let fresh_sp = self.builder.use_var(self.entry_sp_var);
                self.builder.def_var(self.sp_var, fresh_sp);
                let zero = self.builder.ins().f64const(0.0);
                self.builder.def_var(self.counter_var, zero);
                let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
                self.builder.def_var(self.acc_var, undef);
                self.builder.ins().jump(step0, &[]);
                self.builder.seal_block(fresh_block);
                // The resume chain: sp = ctx.resume_sp; compare resume_ip
                // against each static suspension target and jump to its
                // block (a target outside the set returns undefined
                // defensively — the driver only resumes at a continuation).
                self.builder.switch_to_block(resume_block);
                let resume_sp = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    vm,
                    Offset32::new(std::mem::offset_of!(JitCallContext, resume_sp) as i32),
                );
                self.builder.def_var(self.sp_var, resume_sp);
                let zero = self.builder.ins().f64const(0.0);
                self.builder.def_var(self.counter_var, zero);
                let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
                self.builder.def_var(self.acc_var, undef);
                self.emit_resume_chain(resume_ip);
                self.builder.seal_block(resume_block);
                self.builder.seal_block(normal_block);
                self.builder.seal_block(mach_block);
                self.builder.seal_block(throw_block);
                self.builder.seal_block(return_block);
                self.builder.seal_block(entry);
            } else if has_self_tail_call {
                let reentry = self.builder.create_block();
                self.reentry_block = Some(reentry);
                self.builder.ins().jump(reentry, &[]);
                self.builder.seal_block(entry);
                self.builder.switch_to_block(reentry);
                let fresh_sp = self.builder.use_var(self.entry_sp_var);
                self.builder.def_var(self.sp_var, fresh_sp);
                let zero = self.builder.ins().f64const(0.0);
                self.builder.def_var(self.counter_var, zero);
                let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
                self.builder.def_var(self.acc_var, undef);
                self.builder.ins().jump(step0, &[]);
            }
            // Step 0's block is a plain block here (the entry above carries
            // the parameters); it must not be sealed until `seal_all_blocks`
            // because the fresh/re-entry path and (for a self-tail-call) the
            // back edge reach it later.
            self.builder.switch_to_block(step0);
        } else {
            // The common path: bind the parameters and seed the scratch
            // variables directly in the entry block, so a malformed body can
            // never read an undefined variable (the compiler only emits
            // counter/acc uses inside counter loops / register bodies).
            self.visit(0);
            let zero = self.builder.ins().f64const(0.0);
            self.builder.def_var(self.counter_var, zero);
            let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
            self.builder.def_var(self.acc_var, undef);
        }
        for (index, step) in body.steps.iter().enumerate() {
            if index > 0 {
                self.visit(index);
            }
            self.emit_step(index, step)?;
        }
        // Past-the-end block: a body that falls off completes `Empty`, which
        // leaf callers observe as `undefined`. An empty body's step-0 block
        // already carries the seeded variables — re-visiting it would trip
        // Cranelift's "fill your block before switching" debug assert (the
        // block is no longer pristine but has no terminator yet).
        if n > 0 {
            self.visit(n);
        }
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

    /// The interned "length" atom, cached once: the typed-array length probe
    /// applies to `GetMemberName` sites whose name is exactly `length` (the
    /// interpreter's fast path uses the same atom).
    fn length_atom() -> crux::AtomId {
        use std::sync::OnceLock;
        static LENGTH: OnceLock<crux::AtomId> = OnceLock::new();
        *LENGTH.get_or_init(|| crux::string::intern_utf8("length"))
    }

    /// The Cut 38 member-value cell probe, shared by the step path's
    /// `GetMemberName` and the register body's `GetMemberName`/
    /// `GetMemberNameLocal` (the fast-loop shape): check `object` is a
    /// plain Object value, extract the box address, read its LIVE
    /// id/generation, and probe the direct-mapped value cell (recorded by
    /// the interpreter's own-property reads, including the map path) — a
    /// hit serves the cached value with no `get_member_name` helper call.
    /// Function receivers, primitives (boxing), proxies, accessors, and
    /// prototype-chain reads miss to the helper, which re-resolves and
    /// repopulates the cell for the next read. The probe's merge block is
    /// sealed and current on return.
    fn emit_member_cell_read(
        &mut self,
        object: ClifValue,
        name: crux::AtomId,
    ) -> Result<ClifValue, Unsupported> {
        let name_imm = self.builder.ins().iconst(types::I64, name as i64);
        let ctx = self.vm();
        let cells = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            ctx,
            Offset32::new(std::mem::offset_of!(JitCallContext, member_value_cells) as i32),
        );
        let value_var = self.builder.declare_var(types::I64);
        let probe = self.builder.create_block();
        let slow = self.builder.create_block();
        let merge = self.builder.create_block();
        // The typed-array length probe: only the `length` atom needs it. The
        // machine read (gap-close M6) serves a fixed, live, writable-buffer
        // IntegerIndexed receiver's length straight from the slots; a helper
        // covers every other receiver (and the shadowed/auto/detached/
        // resizable shapes), returning the slots length for an
        // IntegerIndexed receiver or the canonical-NaN sentinel. A hit
        // serves the value with no `get_member_name` call (the
        // interpreter's `get_member_name` fast path serves the same slots
        // length, gated on the own-`length` shadow — the probe is exact for
        // the same receivers). Any other receiver falls into the
        // member-cell probe below.
        if name == Self::length_atom() {
            // The machine length read first (M6); its misses land here, the
            // exact FFI probe (which also covers the receivers whose
            // `typed_array` mirror an own-`length` define cleared).
            let ffi = self.builder.create_block();
            self.emit_typed_array_length_inline(object, value_var, ffi, merge)?;
            self.builder.switch_to_block(ffi);
            let len = self.emit_raw_call(self.sig_bool, Helper::TypedArrayLength, &[object])?;
            let sentinel = self
                .builder
                .ins()
                .iconst(types::I64, TYPED_ARRAY_LENGTH_SENTINEL as i64);
            let hit = self.builder.ins().icmp(IntCC::NotEqual, len, sentinel);
            self.builder.def_var(value_var, len);
            let len_miss = self.builder.create_block();
            self.builder.ins().brif(hit, merge, &[], len_miss, &[]);
            self.builder.seal_block(ffi);
            self.builder.switch_to_block(len_miss);
        }
        let is_heap = self.builder.ins().band_imm_u(object, crux::TAG_MASK as i64);
        let is_obj = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, is_heap, crux::TAG_PREFIX as i64);
        let tag = self.builder.ins().ushr_imm_u(object, 44);
        let tag = self.builder.ins().band_imm_u(tag, 0xF);
        let tag_obj = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, tag, crux::TAG_OBJECT as i64);
        let is_plain_obj = self.builder.ins().band(is_obj, tag_obj);
        self.builder.ins().brif(is_plain_obj, probe, &[], slow, &[]);
        // The probe: the cell's id/name/generation must match the
        // receiver's LIVE id and generation (a mutation anywhere bumps the
        // generation, so a match means the own data property is unchanged
        // since the cell was recorded).
        self.builder.switch_to_block(probe);
        let ptr = self
            .builder
            .ins()
            .band_imm_u(object, crux::PAYLOAD_MASK as i64);
        let ptr = self.builder.ins().ishl_imm_u(ptr, 4);
        // The payload stores the `GcBox` base; the `JsObject` sits after
        // the box header (`mark` + `size`).
        let ptr = self
            .builder
            .ins()
            .iadd_imm_s(ptr, crux::heap::GCBOX_DATA_OFFSET as i64);
        let live_id = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            ptr,
            Offset32::new(std::mem::offset_of!(crux::JsObject, id) as i32),
        );
        let live_gen = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            ptr,
            Offset32::new(std::mem::offset_of!(crux::JsObject, generation) as i32),
        );
        let cell_slot = self.builder.ins().bxor(live_id, name_imm);
        let cell_slot = self
            .builder
            .ins()
            .band_imm_u(cell_slot, (MEMBER_CELLS - 1) as i64);
        let index_bytes = self
            .builder
            .ins()
            .imul_imm_s(cell_slot, std::mem::size_of::<MemberValueCell>() as i64);
        let cell = self.builder.ins().iadd(cells, index_bytes);
        let cell_id = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            cell,
            Offset32::new(std::mem::offset_of!(MemberValueCell, id) as i32),
        );
        let cell_name = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            cell,
            Offset32::new(std::mem::offset_of!(MemberValueCell, name) as i32),
        );
        let cell_gen = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            cell,
            Offset32::new(std::mem::offset_of!(MemberValueCell, generation) as i32),
        );
        let cell_value = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            cell,
            Offset32::new(std::mem::offset_of!(MemberValueCell, value) as i32),
        );
        let id_ok = self.builder.ins().icmp(IntCC::Equal, cell_id, live_id);
        let name_ok = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, cell_name, name as i64);
        let gen_ok = self.builder.ins().icmp(IntCC::Equal, cell_gen, live_gen);
        let id_name_ok = self.builder.ins().band(id_ok, name_ok);
        let ok = self.builder.ins().band(id_name_ok, gen_ok);
        self.builder.def_var(value_var, cell_value);
        self.builder.ins().brif(ok, merge, &[], slow, &[]);
        // The slow path: the full Get (which also re-resolves and
        // repopulates the cell for the next read).
        self.builder.switch_to_block(slow);
        let res = self.call_slow(
            self.sig_get_name,
            Helper::GetMemberName,
            &[object, name_imm],
        )?;
        self.builder.def_var(value_var, res);
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(merge);
        self.builder.switch_to_block(merge);
        Ok(self.builder.use_var(value_var))
    }

    /// The machine typed-array length read (gap-close M6), shared by every
    /// compiled `GetMemberName` site whose name is `length` (the step and
    /// register paths both lower through [`Self::emit_member_cell_read`]):
    /// gate the receiver to an Integer-Indexed object over a fixed, live,
    /// writable buffer (the `typed_array` mirror — cleared when an own
    /// `length` shadows the accessor, so the gate implies the accessor
    /// semantics), then serve `slots.array_length` as a Number straight
    /// from the box — no per-read FFI probe. The gates re-read the live
    /// buffer geometry every read, so a helper that detaches or resizes the
    /// buffer between reads is picked up (the mirror + fixed-view checks are
    /// exact for the same receivers the interpreter's `get_member_name`
    /// shortcut serves). Any gate failure jumps to `miss` — the caller's
    /// exact FFI probe (`typed_array_length`), which covers the auto/
    /// detached/resizable/own-length-shadow receivers. The internal blocks
    /// are created and sealed here; on return the current block is the
    /// sealed hit block (it defined `value_var` and jumped to `merge`).
    ///
    /// Emitted only in the single-agent build (`crux::typed_array::WORKERS`
    /// false): under `workers` the block layout is cfg'd and the read would
    /// need atomics, so the probe collapses to the miss jump.
    fn emit_typed_array_length_inline(
        &mut self,
        object: ClifValue,
        value_var: Variable,
        miss: Block,
        merge: Block,
    ) -> Result<(), Unsupported> {
        if crux::typed_array::WORKERS {
            self.builder.ins().jump(miss, &[]);
            return Ok(());
        }
        let inline = self.builder.create_block();
        let mirror = self.builder.create_block();
        let state = self.builder.create_block();
        let hit = self.builder.create_block();
        self.builder.ins().jump(inline, &[]);
        // Object tag: heap prefix + Object tag (same gate as the inline
        // element store — the receiver may be a plain value).
        self.builder.switch_to_block(inline);
        let is_heap = self.builder.ins().band_imm_u(object, crux::TAG_MASK as i64);
        let is_heap = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, is_heap, crux::TAG_PREFIX as i64);
        let tag = self.builder.ins().ushr_imm_u(object, 44);
        let tag = self.builder.ins().band_imm_u(tag, 0xF);
        let tag_obj = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, tag, crux::TAG_OBJECT as i64);
        let obj_ok = self.builder.ins().band(is_heap, tag_obj);
        self.builder.ins().brif(obj_ok, mirror, &[], miss, &[]);
        self.builder.seal_block(inline);
        // The typed-array gate: the object's `typed_array` cell (the slots
        // box base, or 0 when the object is not Integer-Indexed — or an own
        // `length` define cleared it).
        self.builder.switch_to_block(mirror);
        let obj_ptr = self
            .builder
            .ins()
            .band_imm_u(object, crux::PAYLOAD_MASK as i64);
        let obj_ptr = self.builder.ins().ishl_imm_u(obj_ptr, 4);
        let obj_ptr = self
            .builder
            .ins()
            .iadd_imm_s(obj_ptr, crux::heap::GCBOX_DATA_OFFSET as i64);
        let slots_base = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            obj_ptr,
            Offset32::new(std::mem::offset_of!(crux::JsObject, typed_array) as i32),
        );
        let slots_ok = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::NotEqual, slots_base, 0);
        self.builder.ins().brif(slots_ok, state, &[], miss, &[]);
        self.builder.seal_block(mirror);
        // The fixed-view gate: not a length-tracking (auto) view over a
        // detached or resizable buffer — the effective length of such views
        // tracks the live buffer, so the FFI probe computes it.
        self.builder.switch_to_block(state);
        let slots_ptr = self
            .builder
            .ins()
            .iadd_imm_s(slots_base, crux::heap::GCBOX_DATA_OFFSET as i64);
        let auto_len = self.builder.ins().load(
            types::I8,
            MemFlagsData::new(),
            slots_ptr,
            Offset32::new(std::mem::offset_of!(crux::object::TypedArraySlots, auto_length) as i32),
        );
        const STATE_OFFSET: usize = std::mem::offset_of!(crux::object::TypedArraySlots, buffer)
            + std::mem::offset_of!(crux::typed_array::SharedBuffer, state);
        let state_addr = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            slots_ptr,
            Offset32::new(STATE_OFFSET as i32),
        );
        let detached = self.builder.ins().load(
            types::I8,
            MemFlagsData::new(),
            state_addr,
            Offset32::new(std::mem::offset_of!(crux::typed_array::BlockState, detached) as i32),
        );
        let resizable = self.builder.ins().load(
            types::I8,
            MemFlagsData::new(),
            state_addr,
            Offset32::new(std::mem::offset_of!(crux::typed_array::BlockState, resizable) as i32),
        );
        let arr_len = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            slots_ptr,
            Offset32::new(std::mem::offset_of!(crux::object::TypedArraySlots, array_length) as i32),
        );
        let auto_ok = self.builder.ins().icmp_imm_u(IntCC::Equal, auto_len, 0);
        let det_ok = self.builder.ins().icmp_imm_u(IntCC::Equal, detached, 0);
        let res_ok = self.builder.ins().icmp_imm_u(IntCC::Equal, resizable, 0);
        let fixed = self.builder.ins().band(auto_ok, det_ok);
        let fixed = self.builder.ins().band(fixed, res_ok);
        self.builder.ins().brif(fixed, hit, &[], miss, &[]);
        self.builder.seal_block(state);
        // The value: `array_length` as a Number (a small non-negative f64,
        // whose bits are the raw NaN-boxed Number — no reserved-tag form).
        self.builder.switch_to_block(hit);
        let len_f = self.builder.ins().fcvt_from_uint(types::F64, arr_len);
        let len_bits = self
            .builder
            .ins()
            .bitcast(types::I64, MemFlagsData::new(), len_f);
        self.builder.def_var(value_var, len_bits);
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(hit);
        Ok(())
    }

    /// The inline dense-array append gate (gap-close M1 C), shared by the
    /// step `AssignMemberComputed` and the register `StoreMemberComputed`
    /// emissions: verify the receiver is a dense Array (`array_dense`), the
    /// key a canonical index Number equal to `slots.length`, then run the
    /// stateful append through the narrow `dense_array_append` helper. A
    /// stored append jumps to `merge`; any gate failure or a helper 0 jumps
    /// to `legacy` (the caller emits its fallback there). The gate blocks
    /// are created and sealed; on return the current block is the sealed
    /// `append` block — the caller then emits `legacy` and switches to
    /// `merge` (both created by the caller; their predecessors are the
    /// gate's exits plus the caller's legacy fall-through, so they seal at
    /// `seal_all_blocks`).
    fn emit_dense_array_append_inline(
        &mut self,
        object: ClifValue,
        key: ClifValue,
        value: ClifValue,
        legacy: Block,
        merge: Block,
    ) -> Result<(), Unsupported> {
        let inline = self.builder.create_block();
        let probe = self.builder.create_block();
        let key_check = self.builder.create_block();
        let key_ok_check = self.builder.create_block();
        let len_check = self.builder.create_block();
        let append = self.builder.create_block();
        let idx_var = self.builder.declare_var(types::I64);
        self.builder.ins().jump(inline, &[]);
        // Object tag: heap prefix + Object tag.
        self.builder.switch_to_block(inline);
        let is_heap = self.builder.ins().band_imm_u(object, crux::TAG_MASK as i64);
        let is_obj = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, is_heap, crux::TAG_PREFIX as i64);
        let tag = self.builder.ins().ushr_imm_u(object, 44);
        let tag = self.builder.ins().band_imm_u(tag, 0xF);
        let tag_obj = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, tag, crux::TAG_OBJECT as i64);
        let obj_ok = self.builder.ins().band(is_obj, tag_obj);
        self.builder.ins().brif(obj_ok, probe, &[], legacy, &[]);
        self.builder.seal_block(inline);
        // The dense gate: the object's `array_dense` cell (the ArraySlots
        // box base, or 0 when not a dense Array).
        self.builder.switch_to_block(probe);
        let obj_ptr = self
            .builder
            .ins()
            .band_imm_u(object, crux::PAYLOAD_MASK as i64);
        let obj_ptr = self.builder.ins().ishl_imm_u(obj_ptr, 4);
        let obj_ptr = self
            .builder
            .ins()
            .iadd_imm_s(obj_ptr, crux::heap::GCBOX_DATA_OFFSET as i64);
        let slots_base = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            obj_ptr,
            Offset32::new(std::mem::offset_of!(crux::JsObject, array_dense) as i32),
        );
        let slots_ok = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::NotEqual, slots_base, 0);
        self.builder
            .ins()
            .brif(slots_ok, key_check, &[], legacy, &[]);
        self.builder.seal_block(probe);
        // The key must be a double BEFORE the float math — `fcvt_to_uint`
        // on a NaN-boxed heap value's bits (a string key) can trap in the
        // lowering, so the is-double gate is its own block.
        self.builder.switch_to_block(key_check);
        let is_double = self.is_double(key);
        self.builder
            .ins()
            .brif(is_double, key_ok_check, &[], legacy, &[]);
        self.builder.seal_block(key_check);
        // The canonical-index checks: no fraction, 0 <= n < 2^32-1.
        self.builder.switch_to_block(key_ok_check);
        let num = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), key);
        let zero = self.builder.ins().f64const(0.0);
        let max = self.builder.ins().f64const(4294967295.0);
        let ge0 = self
            .builder
            .ins()
            .fcmp(FloatCC::GreaterThanOrEqual, num, zero);
        let lt_max = self.builder.ins().fcmp(FloatCC::LessThan, num, max);
        let idx = self.builder.ins().fcvt_to_uint(types::I64, num);
        let back = self.builder.ins().fcvt_from_uint(types::F64, idx);
        let integral = self.builder.ins().fcmp(FloatCC::Equal, back, num);
        let in_range = self.builder.ins().band(ge0, lt_max);
        let key_ok = self.builder.ins().band(in_range, integral);
        self.builder.def_var(idx_var, idx);
        self.builder.ins().brif(key_ok, len_check, &[], legacy, &[]);
        self.builder.seal_block(key_ok_check);
        // The append gate: index == slots.length (the key is the canonical
        // dense-append position; updates, hole-fills, and grows fall
        // through to the caller's legacy path).
        self.builder.switch_to_block(len_check);
        let slots_ptr = self
            .builder
            .ins()
            .iadd_imm_s(slots_base, crux::heap::GCBOX_DATA_OFFSET as i64);
        let length = self.builder.ins().load(
            types::F64,
            MemFlagsData::new(),
            slots_ptr,
            Offset32::new(std::mem::offset_of!(crux::object::ArraySlots, length) as i32),
        );
        let idx = self.builder.use_var(idx_var);
        let idx_f = self.builder.ins().fcvt_from_uint(types::F64, idx);
        let append_ok = self.builder.ins().fcmp(FloatCC::Equal, idx_f, length);
        self.builder.ins().brif(append_ok, append, &[], legacy, &[]);
        self.builder.seal_block(len_check);
        // The narrow append helper (raw — it cannot set the pending byte).
        self.builder.switch_to_block(append);
        let idx = self.builder.use_var(idx_var);
        let ok = self.emit_raw_call(
            self.sig_binary,
            Helper::DenseArrayAppend,
            &[object, idx, value],
        )?;
        let hit = self.builder.ins().icmp_imm_u(IntCC::Equal, ok, 1);
        self.builder.ins().brif(hit, merge, &[], legacy, &[]);
        self.builder.seal_block(append);
        Ok(())
    }

    /// The machine typed-array element store (gap-close M5c), shared by the
    /// step `AssignMemberComputed` and the register `StoreMemberComputed`
    /// emissions: gate the receiver to a fixed-length `Uint8Array` over a
    /// live, writable, non-resizable buffer, the key to a canonical index
    /// Number below `slots.array_length`, and the value to an integral
    /// Number in [0, 255]; then write the byte straight into the block at
    /// `data + byte_offset + index`.
    ///
    /// The gate re-reads the live buffer geometry (the byte base and the
    /// detached/immutable/resizable flags) from the shared `BlockState` box
    /// on every store, so a helper that detaches, freezes, or resizes the
    /// buffer between stores is picked up; the machine write is a pure byte
    /// store (no coercion ran — the value is a Number, and the write never
    /// runs user code), so a gate failure can always fall back to `legacy`
    /// (the caller re-runs the full IntegerIndexedElementSet and nothing
    /// observable happened on the fast attempt).
    ///
    /// Emitted only in the single-agent build (`crux::typed_array::WORKERS`
    /// false): under `workers` the block layout is cfg'd and the write would
    /// need atomics, so the whole probe collapses to the legacy jump. The
    /// internal blocks are created and sealed here; on return the current
    /// block is the sealed `store` block — the caller emits `legacy` and
    /// switches to `merge` (both created by the caller; their predecessors
    /// are this gate's exits plus the caller's other paths, so they seal at
    /// `seal_all_blocks`).
    fn emit_typed_array_store_inline(
        &mut self,
        object: ClifValue,
        key: ClifValue,
        value: ClifValue,
        legacy: Block,
        merge: Block,
    ) -> Result<(), Unsupported> {
        if crux::typed_array::WORKERS {
            self.builder.ins().jump(legacy, &[]);
            return Ok(());
        }
        let tag = self.builder.create_block();
        let probe = self.builder.create_block();
        let loads = self.builder.create_block();
        let key_range = self.builder.create_block();
        let val_range = self.builder.create_block();
        let ints = self.builder.create_block();
        let store = self.builder.create_block();
        self.builder.ins().jump(tag, &[]);
        // Object tag: heap prefix + Object tag (same gate as the dense
        // append — the object here may be a plain value).
        self.builder.switch_to_block(tag);
        let is_heap = self.builder.ins().band_imm_u(object, crux::TAG_MASK as i64);
        let is_heap = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, is_heap, crux::TAG_PREFIX as i64);
        let tag_bits = self.builder.ins().ushr_imm_u(object, 44);
        let tag_bits = self.builder.ins().band_imm_u(tag_bits, 0xF);
        let tag_obj =
            self.builder
                .ins()
                .icmp_imm_u(IntCC::Equal, tag_bits, crux::TAG_OBJECT as i64);
        let obj_ok = self.builder.ins().band(is_heap, tag_obj);
        self.builder.ins().brif(obj_ok, probe, &[], legacy, &[]);
        self.builder.seal_block(tag);
        // The typed-array gate: the object's `typed_array` cell (the slots
        // box base, or 0 when the object is not Integer-Indexed).
        self.builder.switch_to_block(probe);
        let obj_ptr = self
            .builder
            .ins()
            .band_imm_u(object, crux::PAYLOAD_MASK as i64);
        let obj_ptr = self.builder.ins().ishl_imm_u(obj_ptr, 4);
        let obj_ptr = self
            .builder
            .ins()
            .iadd_imm_s(obj_ptr, crux::heap::GCBOX_DATA_OFFSET as i64);
        let slots_base = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            obj_ptr,
            Offset32::new(std::mem::offset_of!(crux::JsObject, typed_array) as i32),
        );
        let slots_ok = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::NotEqual, slots_base, 0);
        self.builder.ins().brif(slots_ok, loads, &[], legacy, &[]);
        self.builder.seal_block(probe);
        // The geometry block: all the per-store loads issue together, right
        // after the slots pointer is known, so their latencies overlap the
        // key/value gates below. The element type (Uint8 only — a
        // byte-valued Number writes its own byte), the buffer geometry from
        // the shared BlockState box (the live data base; not
        // detached/immutable/resizable — a helper that detaches, freezes, or
        // resizes the buffer mid-run is picked up here), the view's fixed
        // length, and the byte offset.
        self.builder.switch_to_block(loads);
        let slots_ptr = self
            .builder
            .ins()
            .iadd_imm_s(slots_base, crux::heap::GCBOX_DATA_OFFSET as i64);
        let etype = self.builder.ins().load(
            types::I8,
            MemFlagsData::new(),
            slots_ptr,
            Offset32::new(std::mem::offset_of!(crux::object::TypedArraySlots, element_type) as i32),
        );
        let is_u8 = self.builder.ins().icmp_imm_u(
            IntCC::Equal,
            etype,
            crux::typed_array::ElementType::Uint8 as u8 as i64,
        );
        const STATE_OFFSET: usize = std::mem::offset_of!(crux::object::TypedArraySlots, buffer)
            + std::mem::offset_of!(crux::typed_array::SharedBuffer, state);
        let state_addr = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            slots_ptr,
            Offset32::new(STATE_OFFSET as i32),
        );
        let data = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            state_addr,
            Offset32::new(std::mem::offset_of!(crux::typed_array::BlockState, data) as i32),
        );
        let detached = self.builder.ins().load(
            types::I8,
            MemFlagsData::new(),
            state_addr,
            Offset32::new(std::mem::offset_of!(crux::typed_array::BlockState, detached) as i32),
        );
        let immutable = self.builder.ins().load(
            types::I8,
            MemFlagsData::new(),
            state_addr,
            Offset32::new(std::mem::offset_of!(crux::typed_array::BlockState, immutable) as i32),
        );
        let resizable = self.builder.ins().load(
            types::I8,
            MemFlagsData::new(),
            state_addr,
            Offset32::new(std::mem::offset_of!(crux::typed_array::BlockState, resizable) as i32),
        );
        let arr_len = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            slots_ptr,
            Offset32::new(std::mem::offset_of!(crux::object::TypedArraySlots, array_length) as i32),
        );
        let byte_offset = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            slots_ptr,
            Offset32::new(std::mem::offset_of!(crux::object::TypedArraySlots, byte_offset) as i32),
        );
        let det_ok = self.builder.ins().icmp_imm_u(IntCC::Equal, detached, 0);
        let imm_ok = self.builder.ins().icmp_imm_u(IntCC::Equal, immutable, 0);
        let res_ok = self.builder.ins().icmp_imm_u(IntCC::Equal, resizable, 0);
        let clear_ok = self.builder.ins().band(det_ok, imm_ok);
        let clear_ok = self.builder.ins().band(clear_ok, res_ok);
        let has_data = self.builder.ins().icmp_imm_u(IntCC::NotEqual, data, 0);
        let geom_ok = self.builder.ins().band(is_u8, clear_ok);
        let geom_ok = self.builder.ins().band(geom_ok, has_data);
        self.builder
            .ins()
            .brif(geom_ok, key_range, &[], legacy, &[]);
        self.builder.seal_block(loads);
        // The key must be a real double in the canonical index range BEFORE
        // any float-to-int conversion: the conversion would trap on the
        // NaN-boxed bits of a heap value (a string key), so the range gate
        // (which only a real f64 can pass — a heap value's bits decode as
        // NaN) runs first, in its own block, before the fcvt in `ints`.
        self.builder.switch_to_block(key_range);
        let num = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), key);
        let zero = self.builder.ins().f64const(0.0);
        let max = self.builder.ins().f64const(4294967295.0);
        let ge0 = self
            .builder
            .ins()
            .fcmp(FloatCC::GreaterThanOrEqual, num, zero);
        let lt_max = self.builder.ins().fcmp(FloatCC::LessThan, num, max);
        let key_rng = self.builder.ins().band(ge0, lt_max);
        self.builder
            .ins()
            .brif(key_rng, val_range, &[], legacy, &[]);
        self.builder.seal_block(key_range);
        // The value must be a real f64 in [0, 255] (a byte-valued Number).
        // Object/Function/String values would run user code or a conversion
        // in the coercion — the legacy helper performs those with the exact
        // observable behavior. The range gate doubles as the is-double gate:
        // non-double bits decode as NaN, which fails the range.
        self.builder.switch_to_block(val_range);
        let vnum = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), value);
        let v255 = self.builder.ins().f64const(255.0);
        let vge0 = self
            .builder
            .ins()
            .fcmp(FloatCC::GreaterThanOrEqual, vnum, zero);
        let vle255 = self
            .builder
            .ins()
            .fcmp(FloatCC::LessThanOrEqual, vnum, v255);
        let vrange = self.builder.ins().band(vge0, vle255);
        self.builder.ins().brif(vrange, ints, &[], legacy, &[]);
        self.builder.seal_block(val_range);
        // The conversions + the final combine: the key/value integrality
        // round trips (they reject 3.5 etc. — the conversions are trap-free
        // here, the range gates already passed) and the index-vs-fixed-
        // length bound.
        self.builder.switch_to_block(ints);
        let idx = self.builder.ins().fcvt_to_uint(types::I64, num);
        let back = self.builder.ins().fcvt_from_uint(types::F64, idx);
        let key_int = self.builder.ins().fcmp(FloatCC::Equal, back, num);
        let vbyte = self.builder.ins().fcvt_to_uint(types::I64, vnum);
        let vback = self.builder.ins().fcvt_from_uint(types::F64, vbyte);
        let val_int = self.builder.ins().fcmp(FloatCC::Equal, vback, vnum);
        let len_ok = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, idx, arr_len);
        let ok = self.builder.ins().band(key_int, val_int);
        let ok = self.builder.ins().band(ok, len_ok);
        self.builder.ins().brif(ok, store, &[], legacy, &[]);
        self.builder.seal_block(ints);
        // The store: one byte at data + byte_offset + index.
        self.builder.switch_to_block(store);
        let base = self.builder.ins().iadd(data, byte_offset);
        let addr = self.builder.ins().iadd(base, idx);
        let byte = self.builder.ins().ireduce(types::I8, vbyte);
        self.builder
            .ins()
            .store(MemFlagsData::new(), byte, addr, Offset32::new(0));
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(store);
        Ok(())
    }

    /// Cut 36: the direct-mapped global-value fast-cell read (the inline
    /// `LoadGlobal` kernel): validate the cell's name and captured version
    /// against the global object's LIVE identity/generation (re-read from
    /// the per-call context), so a helper that mutated the global mid-run
    /// bumps the generation and the fast path misses to `get_global` —
    /// which re-resolves and repopulates the cell. A null global (a bare
    /// test context) also falls through. The merge block is sealed and
    /// current on return. Used by `LoadGlobal` and the `TailCallFastGlobal`
    /// callee read.
    fn emit_global_read(&mut self, name: crux::AtomId) -> Result<ClifValue, Unsupported> {
        let ctx = self.vm();
        let global = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            ctx,
            Offset32::new(std::mem::offset_of!(JitCallContext, global_object) as i32),
        );
        let cells = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            ctx,
            Offset32::new(std::mem::offset_of!(JitCallContext, global_value_cells) as i32),
        );
        let name_imm = self.builder.ins().iconst(types::I64, name as i64);
        let value_var = self.builder.declare_var(types::I64);
        let slow = self.builder.create_block();
        let merge = self.builder.create_block();
        let has_global = self.builder.ins().icmp_imm_u(IntCC::NotEqual, global, 0);
        let has_global_64 = self.bint(has_global);
        let probe = self.builder.create_block();
        self.builder
            .ins()
            .brif(has_global_64, probe, &[], slow, &[]);
        // The fast path: the cell's name and captured version must match the
        // live global (a stale cell, another realm's global, or a mid-run
        // mutation all miss).
        self.builder.switch_to_block(probe);
        let live_id = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            global,
            Offset32::new(std::mem::offset_of!(crux::JsObject, id) as i32),
        );
        let live_gen = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            global,
            Offset32::new(std::mem::offset_of!(crux::JsObject, generation) as i32),
        );
        let cell = self.builder.ins().iadd_imm_s(
            cells,
            ((name as usize & (GLOBAL_CELLS - 1)) * std::mem::size_of::<GlobalValueCell>()) as i64,
        );
        let cell_name = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            cell,
            Offset32::new(std::mem::offset_of!(GlobalValueCell, name) as i32),
        );
        let cell_id = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            cell,
            Offset32::new(std::mem::offset_of!(GlobalValueCell, global_id) as i32),
        );
        let cell_gen = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            cell,
            Offset32::new(std::mem::offset_of!(GlobalValueCell, generation) as i32),
        );
        let cell_value = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            cell,
            Offset32::new(std::mem::offset_of!(GlobalValueCell, value) as i32),
        );
        let name_ok = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, cell_name, name as i64);
        let id_ok = self.builder.ins().icmp(IntCC::Equal, cell_id, live_id);
        let gen_ok = self.builder.ins().icmp(IntCC::Equal, cell_gen, live_gen);
        let name_ok_64 = self.bint(name_ok);
        let id_ok_64 = self.bint(id_ok);
        let gen_ok_64 = self.bint(gen_ok);
        let name_id_ok = self.builder.ins().band(name_ok_64, id_ok_64);
        let ok = self.builder.ins().band(name_id_ok, gen_ok_64);
        self.builder.def_var(value_var, cell_value);
        self.builder.ins().brif(ok, merge, &[], slow, &[]);
        // The slow path: re-resolve through the runtime (which also
        // repopulates the cell for the next read).
        self.builder.switch_to_block(slow);
        let res = self.call_slow(self.sig_bool, Helper::GetGlobal, &[name_imm])?;
        self.builder.def_var(value_var, res);
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(merge);
        self.builder.switch_to_block(merge);
        Ok(self.builder.use_var(value_var))
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

    /// Load the running `Vm` pointer (the ctx's `vm` field) — the machine
    /// code's handle for writing the interpreter's completion register.
    fn vm_ptr(&mut self) -> ClifValue {
        let ctx = self.vm();
        self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            ctx,
            Offset32::new(std::mem::offset_of!(JitCallContext, vm) as i32),
        )
    }

    /// Write the interpreter's completion register (`Vm::completion` plus
    /// `completion_is_empty`) from machine code — the script path's
    /// fall-off-end completion reads it (`run_inner_inner`'s `None` arm), and
    /// the interpreter's completion steps are the source of truth the compiled
    /// code must mirror. Both fields are traced/reset with the Vm, so the
    /// stored value stays rooted and the pool clears it between runs. The
    /// write is skipped when the ctx's `vm` pointer is null — the scaffold's
    /// bare-ctx test harness (which never dereferences it) calls the compiled
    /// code with a null `vm`.
    fn emit_completion_store(&mut self, value: ClifValue, is_empty: bool) {
        let vm = self.vm_ptr();
        let has_vm = self.builder.ins().icmp_imm_u(IntCC::NotEqual, vm, 0);
        let has_vm_64 = self.bint(has_vm);
        let store = self.builder.create_block();
        let skip = self.builder.create_block();
        self.builder.ins().brif(has_vm_64, store, &[], skip, &[]);
        self.builder.switch_to_block(store);
        self.builder.ins().store(
            MemFlagsData::new(),
            value,
            vm,
            Offset32::new(VM_COMPLETION_OFFSET as i32),
        );
        let empty = self.builder.ins().iconst(types::I8, is_empty as i64);
        self.builder.ins().store(
            MemFlagsData::new(),
            empty,
            vm,
            Offset32::new(VM_COMPLETION_IS_EMPTY_OFFSET as i32),
        );
        self.builder.ins().jump(skip, &[]);
        self.builder.seal_block(store);
        self.builder.switch_to_block(skip);
        self.builder.seal_block(skip);
    }

    fn call_slow(
        &mut self,
        sig: SigRef,
        helper: Helper,
        args: &[ClifValue],
    ) -> Result<ClifValue, Unsupported> {
        let result = self.emit_raw_call(sig, helper, args)?;
        // The error ABI: a helper that hit an interpreter error sets the
        // context's `pending` byte (offset 0). Without try machinery, bail
        // the whole body out immediately (returning `undefined` — the
        // runtime surfaces the pending error) so no further side effect runs
        // with the placeholder value. With try machinery, the error routes
        // through the handler table like the interpreter's Err arm: a
        // covering catch/finally dispatches to its block, an uncovered one
        // re-sets the pending error and returns.
        let vm = self.vm();
        let pending = self
            .builder
            .ins()
            .load(types::I8, MemFlagsData::new(), vm, Offset32::new(0));
        let ok = self.builder.ins().icmp_imm_u(IntCC::Equal, pending, 0);
        let cont = self.builder.create_block();
        let err = self.builder.create_block();
        self.builder.ins().brif(ok, cont, &[], err, &[]);
        self.builder.switch_to_block(err);
        if self.has_try {
            // A register body's transient stack use is unwound to its entry
            // depth before the error propagates (the interpreter truncates
            // before returning); the catch block reads the sp at the step.
            if let Some(sp) = self.error_sp {
                self.builder.def_var(self.sp_var, sp);
            }
            let ip = self
                .builder
                .ins()
                .iconst(types::I64, (self.current_step + 1) as i64);
            let res = self.emit_raw_call(self.sig_bool, Helper::DispatchError, &[ip])?;
            self.bump_leaf_epoch();
            self.emit_dispatch(res);
        } else if self.has_for_of || self.has_destructure {
            // A helper error inside a for-of/destructuring body escapes with
            // the iterator(s) open on this Vm (the callee's stacks are
            // discarded when the error surfaces) — close them first,
            // mirroring `run_inner`'s uncovered-error close (the destructure
            // close runs first, matching the interpreter's Err arm order).
            // The raw calls skip the pending check (the pending error is
            // already set; the closes swallow or replace per spec 7.4.11).
            if self.has_destructure {
                self.emit_raw_call(self.sig_tdz, Helper::DestructureCloseAll, &[])?;
            }
            if self.has_for_of {
                self.emit_raw_call(self.sig_tdz, Helper::ForOfCloseAll, &[])?;
            }
            let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
            self.builder.ins().return_(&[undef]);
        } else {
            let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
            self.builder.ins().return_(&[undef]);
        }
        self.builder.seal_block(err);
        self.builder.switch_to_block(cont);
        // Cut 39: a helper that can re-enter the interpreter may disturb the
        // Vm stacks / realm count the leaf-call probe's eligibility checks,
        // so bump the leaf-eligibility epoch — any cached leaf verdict from
        // before the helper is invalidated (the next call site re-probes).
        if helper.disturbs_leaf_eligibility() {
            self.bump_leaf_epoch();
        }
        Ok(result)
    }

    /// The raw helper call: no pending check, no epoch bump. Used by
    /// `call_slow` (which adds the error ABI) and the control-dispatch
    /// helpers (which manage their own pending/error state and return a
    /// dispatch code instead of a value).
    fn emit_raw_call(
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

    /// Bump the ctx's leaf-eligibility epoch: the caller's Vm stacks / env
    /// chain / realm count may have changed in a way the leaf-call probe's
    /// cached verdicts assume stable, so any cached verdict must be re-probed.
    fn bump_leaf_epoch(&mut self) {
        let vm = self.vm();
        let epoch = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            vm,
            Offset32::new(std::mem::offset_of!(JitCallContext, leaf_epoch) as i32),
        );
        let bumped = self.builder.ins().iadd_imm_u(epoch, 1);
        self.builder.ins().store(
            MemFlagsData::new(),
            bumped,
            vm,
            Offset32::new(std::mem::offset_of!(JitCallContext, leaf_epoch) as i32),
        );
    }

    /// Emit a control-dispatch helper call: the raw call, an epoch bump (the
    /// dispatch mutates the try/pending/env stacks the leaf-call probe
    /// reads), then the dispatch on the returned code.
    fn emit_dispatch_call(
        &mut self,
        sig: SigRef,
        helper: Helper,
        args: &[ClifValue],
    ) -> Result<(), Unsupported> {
        let res = self.emit_raw_call(sig, helper, args)?;
        self.bump_leaf_epoch();
        self.emit_dispatch(res);
        Ok(())
    }

    /// Interpret a control-dispatch result (Cut 55): `u64::MAX` signals an
    /// escaping throw (the pending error is set — return `undefined` and let
    /// the runtime surface it); `u64::MAX - 1` signals a completed return
    /// (return the value from `ctx.dispatch_value`); any other value is a
    /// step index to jump to — a compare chain over the body's static
    /// transfer targets.
    fn emit_dispatch(&mut self, res: ClifValue) {
        let ctx = self.vm();
        let propagate = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, res, u64::MAX as i64);
        let done = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, res, (u64::MAX - 1) as i64);
        let cont = self.builder.create_block();
        let done_cont = self.builder.create_block();
        let propagate_block = self.builder.create_block();
        let done_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(propagate, propagate_block, &[], cont, &[]);
        self.builder.switch_to_block(propagate_block);
        let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
        self.builder.ins().return_(&[undef]);
        self.builder.seal_block(propagate_block);
        self.builder.switch_to_block(cont);
        self.builder
            .ins()
            .brif(done, done_block, &[], done_cont, &[]);
        self.builder.switch_to_block(done_block);
        let value = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            ctx,
            Offset32::new(std::mem::offset_of!(JitCallContext, dispatch_value) as i32),
        );
        self.builder.ins().return_(&[value]);
        self.builder.seal_block(done_block);
        self.builder.switch_to_block(done_cont);
        self.emit_jump_to_step(res);
    }

    /// Jump to the block for a runtime step index: a compare chain over the
    /// body's static dispatch targets (the control helpers only ever return
    /// an `Exit`'s `after`, a `Break`/`Continue` target, or a handler's
    /// catch/finally start). A target outside the set is an invariant
    /// violation — the machine code returns `undefined` defensively.
    fn emit_jump_to_step(&mut self, target: ClifValue) {
        if self.dispatch_targets.is_empty() {
            let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
            self.builder.ins().return_(&[undef]);
            return;
        }
        let default = self.builder.create_block();
        let mut chain = self.builder.create_block();
        self.builder.ins().jump(chain, &[]);
        let targets: Vec<usize> = self.dispatch_targets.clone();
        for (i, t) in targets.iter().enumerate() {
            self.builder.switch_to_block(chain);
            let eq = self
                .builder
                .ins()
                .icmp_imm_u(IntCC::Equal, target, *t as i64);
            let block = self.ensure_block(*t);
            if i + 1 == self.dispatch_targets.len() {
                self.builder.ins().brif(eq, block, &[], default, &[]);
            } else {
                let next = self.builder.create_block();
                self.builder.ins().brif(eq, block, &[], next, &[]);
                chain = next;
            }
        }
        self.builder.switch_to_block(default);
        let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
        self.builder.ins().return_(&[undef]);
        self.builder.seal_block(default);
    }

    /// Emit a call step with the leaf-inline probe (Cut 37) and its
    /// per-call-site cache (Cut 39): the probe validates the callee (a
    /// this-less, env-free leaf whose compiled body fits in the working
    /// buffer) and fills its frame above the arguments; on a hit the machine
    /// code calls the leaf's compiled entry directly in-frame — no
    /// `call_slow`, no interpreter call machinery, no Vm-stack round trip —
    /// and on a miss falls back to `call_slow` unchanged. A cached verdict
    /// (this site, this callee, unchanged leaf-eligibility epoch) skips the
    /// probe helper entirely on repeat visits. `pre_call_sp` is the stack
    /// pointer the result replaces (the call's argument region base).
    /// Cut 68: re-validate the leaf-eligibility state is AT REST — the
    /// `Vm::can_inline_leaf` conditions plus the realm count, exactly what
    /// `leaf_call_probe` checks. The compiled leaf-cache gate re-runs this
    /// when the epoch is stale (a disturbing helper ran) instead of
    /// re-probing: if the state never left at-rest, the cached verdict —
    /// positive (the entry is a pure function of the unchanged callee) or
    /// negative (the slow path is always correct) — is still valid and the
    /// epoch is simply re-stamped.
    fn emit_leaf_state_at_rest(&mut self) -> ClifValue {
        let vm = self.vm_ptr();
        let mut ok = self.builder.ins().iconst(types::I64, 1);
        for offset in [
            VM_TRY_STACK_LEN_OFFSET,
            VM_PENDING_LEN_OFFSET,
            VM_FOR_OF_STACK_LEN_OFFSET,
            VM_FOR_OF_BOUNDARIES_LEN_OFFSET,
            VM_FOR_IN_STACK_LEN_OFFSET,
            VM_ASYNC_FOR_OF_STACK_LEN_OFFSET,
            VM_DESTRUCTURE_STACK_LEN_OFFSET,
        ] {
            let len = self.builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                vm,
                Offset32::new(offset as i32),
            );
            let empty = self.builder.ins().icmp_imm_u(IntCC::Equal, len, 0);
            let empty_64 = self.bint(empty);
            ok = self.builder.ins().band(ok, empty_64);
        }
        let env_len = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            vm,
            Offset32::new(VM_ENV_STACK_LEN_OFFSET as i32),
        );
        let env_ok = self.builder.ins().icmp_imm_u(IntCC::Equal, env_len, 1);
        let env_ok_64 = self.bint(env_ok);
        ok = self.builder.ins().band(ok, env_ok_64);
        let ctx = self.vm();
        let agent = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            ctx,
            Offset32::new(std::mem::offset_of!(JitCallContext, agent) as i32),
        );
        let realm = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            agent,
            Offset32::new(AGENT_REALM_COUNT_OFFSET as i32),
        );
        let realm_ok = self.builder.ins().icmp_imm_u(IntCC::Equal, realm, 1);
        let realm_ok_64 = self.bint(realm_ok);
        self.builder.ins().band(ok, realm_ok_64)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_call(
        &mut self,
        index: usize,
        callee: ClifValue,
        this: ClifValue,
        args_ptr: ClifValue,
        argc: ClifValue,
        pre_call_sp: ClifValue,
        direct_eval: bool,
        emit_fall_through: bool,
    ) -> Result<(), Unsupported> {
        let ctx = self.vm();
        let cache = self
            .builder
            .ins()
            .iadd_imm_s(ctx, leaf_cache_entry_offset(index) as i64);
        // The cache-reuse gate: the record matches this call site, the
        // leaf-eligibility epoch is unchanged since the probe wrote it (no
        // JS-running helper ran), and the live callee's NaN-box identity
        // (`bits >> 44` + `bits & PAYLOAD_MASK`, together all 64 bits)
        // matches the probed callee.
        let site = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            cache,
            Offset32::new(std::mem::offset_of!(LeafCallSiteCache, site) as i32),
        );
        let site_ok = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, site, index as i64);
        let epoch = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            ctx,
            Offset32::new(std::mem::offset_of!(JitCallContext, leaf_epoch) as i32),
        );
        let cached_epoch = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            cache,
            Offset32::new(std::mem::offset_of!(LeafCallSiteCache, epoch) as i32),
        );
        let epoch_ok = self.builder.ins().icmp(IntCC::Equal, epoch, cached_epoch);
        let callee_hi = self.builder.ins().ushr_imm_u(callee, 44);
        let cached_hi = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            cache,
            Offset32::new(std::mem::offset_of!(LeafCallSiteCache, callee_hi) as i32),
        );
        let cached_hi = self.builder.ins().uextend(types::I64, cached_hi);
        let hi_ok = self.builder.ins().icmp(IntCC::Equal, callee_hi, cached_hi);
        let payload = self
            .builder
            .ins()
            .band_imm_u(callee, crux::PAYLOAD_MASK as i64);
        let cached_payload = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            cache,
            Offset32::new(std::mem::offset_of!(LeafCallSiteCache, callee_payload) as i32),
        );
        let payload_ok = self
            .builder
            .ins()
            .icmp(IntCC::Equal, payload, cached_payload);
        let callee_hit = self.builder.ins().band(hi_ok, payload_ok);
        let stable = self.builder.ins().band(site_ok, callee_hit);
        let hit = self.builder.ins().band(stable, epoch_ok);
        let cached_entry = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            cache,
            Offset32::new(leaf_inline_offset(std::mem::offset_of!(LeafInlineInfo, entry)) as i32),
        );
        let hit_block = self.builder.create_block();
        let stable_check = self.builder.create_block();
        let stale_block = self.builder.create_block();
        let probe_block = self.builder.create_block();
        let slow = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(hit, hit_block, &[], stable_check, &[]);
        // Cut 68: a miss is either a different site/callee (probe) or a stale
        // epoch — a disturbing helper (a getter, a `valueOf`, a nested call)
        // ran since the probe. If the eligibility state is STILL at rest (the
        // helper didn't actually change any stack or the realm count), the
        // cached verdict is still valid: re-stamp the epoch and reuse it
        // instead of re-running the full probe every iteration (the
        // monomorphic-hot-call-next-to-a-getter shape previously re-probed
        // per iteration).
        self.builder.switch_to_block(stable_check);
        self.builder
            .ins()
            .brif(stable, stale_block, &[], probe_block, &[]);
        self.builder.switch_to_block(stale_block);
        let state_ok = self.emit_leaf_state_at_rest();
        let restamp = self.builder.create_block();
        self.builder
            .ins()
            .brif(state_ok, restamp, &[], probe_block, &[]);
        self.builder.switch_to_block(restamp);
        self.builder.ins().store(
            MemFlagsData::new(),
            epoch,
            cache,
            Offset32::new(std::mem::offset_of!(LeafCallSiteCache, epoch) as i32),
        );
        self.builder.ins().jump(hit_block, &[]);
        self.builder.seal_block(restamp);
        self.builder.seal_block(stale_block);
        self.builder.seal_block(stable_check);
        // A cache hit: a cached zero entry is a stable rejection (go straight
        // to `call_slow`); a nonzero entry reuses the verdict in-frame.
        self.builder.switch_to_block(hit_block);
        let entry_zero = self.builder.ins().icmp_imm_u(IntCC::Equal, cached_entry, 0);
        let fast = self.builder.create_block();
        self.builder.ins().brif(entry_zero, slow, &[], fast, &[]);
        // The aliased in-frame call: the leaf's frame IS the argument region
        // (`frame_size == arity` with all args present), so there is no frame
        // fill for the machine code to reproduce — a built frame's per-slot
        // TDZ markers come from the probe's fill, which a non-aliased hit
        // falls back to.
        self.builder.switch_to_block(fast);
        let frame_size = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            cache,
            Offset32::new(
                leaf_inline_offset(std::mem::offset_of!(LeafInlineInfo, frame_size)) as i32,
            ),
        );
        let arity = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            cache,
            Offset32::new(leaf_inline_offset(std::mem::offset_of!(LeafInlineInfo, arity)) as i32),
        );
        let stack_usage = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            cache,
            Offset32::new(
                leaf_inline_offset(std::mem::offset_of!(LeafInlineInfo, stack_usage)) as i32,
            ),
        );
        let fs_eq_ar = self.builder.ins().icmp(IntCC::Equal, frame_size, arity);
        let frame_size_64 = self.builder.ins().uextend(types::I64, frame_size);
        let argc_ge_fs =
            self.builder
                .ins()
                .icmp(IntCC::UnsignedLessThanOrEqual, frame_size_64, argc);
        let aliased = self.builder.ins().band(fs_eq_ar, argc_ge_fs);
        let room = self.builder.create_block();
        self.builder
            .ins()
            .brif(aliased, room, &[], probe_block, &[]);
        // The inline frame + working area must fit above the argument
        // region's top in the caller's working buffer (the probe checked it
        // once; the stack pointer at a merged-CFG call site can differ across
        // visits, so re-check before trusting the cached verdict).
        self.builder.switch_to_block(room);
        let buf_end = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            ctx,
            Offset32::new(std::mem::offset_of!(JitCallContext, buf_end) as i32),
        );
        let argc_bytes = self.builder.ins().imul_imm_s(argc, 8);
        let args_top = self.builder.ins().iadd(args_ptr, argc_bytes);
        let stack_bytes = self.builder.ins().imul_imm_s(stack_usage, 8);
        let top_needed = self.builder.ins().iadd(args_top, stack_bytes);
        let fits = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, top_needed, buf_end);
        let inline = self.builder.create_block();
        self.builder.ins().brif(fits, inline, &[], probe_block, &[]);
        self.builder.switch_to_block(inline);
        let frame_size_64 = self.builder.ins().uextend(types::I64, frame_size);
        let frame_bytes = self.builder.ins().imul_imm_s(frame_size_64, 8);
        let stack_ptr = self.builder.ins().iadd(args_ptr, frame_bytes);
        self.emit_leaf_call_tail(cached_entry, args_ptr, stack_ptr, pre_call_sp, merge);
        // The probe path: the full validation + lookups + frame fill (Cut
        // 37); the probe records the cache identity so repeat visits skip it.
        self.builder.switch_to_block(probe_block);
        let site_imm = self.builder.ins().iconst(types::I64, index as i64);
        let probe = self.call_slow(
            self.sig_call,
            Helper::LeafCallProbe,
            &[callee, args_ptr, argc, site_imm],
        )?;
        let hit = self.builder.ins().icmp_imm_u(IntCC::NotEqual, probe, 0);
        let inline2 = self.builder.create_block();
        self.builder.ins().brif(hit, inline2, &[], slow, &[]);
        self.builder.switch_to_block(inline2);
        let ctx = self.vm();
        let info = self.builder.ins().iadd_imm_s(
            ctx,
            (leaf_cache_entry_offset(index) + std::mem::offset_of!(LeafCallSiteCache, leaf_inline))
                as i64,
        );
        let frame_size = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            info,
            Offset32::new(std::mem::offset_of!(LeafInlineInfo, frame_size) as i32),
        );
        let arity = self.builder.ins().load(
            types::I32,
            MemFlagsData::new(),
            info,
            Offset32::new(std::mem::offset_of!(LeafInlineInfo, arity) as i32),
        );
        let sp = self.builder.use_var(self.sp_var);
        let fs_eq_ar = self.builder.ins().icmp(IntCC::Equal, frame_size, arity);
        let frame_size_64 = self.builder.ins().uextend(types::I64, frame_size);
        let argc_ge_fs =
            self.builder
                .ins()
                .icmp(IntCC::UnsignedLessThanOrEqual, frame_size_64, argc);
        let aliased = self.builder.ins().band(fs_eq_ar, argc_ge_fs);
        let frame_ptr = self.builder.ins().select(aliased, args_ptr, sp);
        let frame_size_64 = self.builder.ins().uextend(types::I64, frame_size);
        let frame_bytes = self.builder.ins().imul_imm_s(frame_size_64, 8);
        let stack_ptr = self.builder.ins().iadd(frame_ptr, frame_bytes);
        self.emit_leaf_call_tail(probe, frame_ptr, stack_ptr, pre_call_sp, merge);
        // The slow path: the interpreter's call machinery (unchanged). The
        // `direct_eval` flag rides along (Cut 62): a direct-eval callee
        // must run `perform_eval` with the caller's environment intact.
        self.builder.switch_to_block(slow);
        let direct_eval_imm = self
            .builder
            .ins()
            .iconst(types::I64, i64::from(direct_eval));
        let res = self.call_slow(
            self.sig_call_slow,
            Helper::CallSlow,
            &[callee, this, argc, args_ptr, direct_eval_imm],
        )?;
        self.builder.def_var(self.sp_var, pre_call_sp);
        self.push(res);
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(merge);
        self.builder.switch_to_block(merge);
        if emit_fall_through {
            self.fall_through(index);
        }
        Ok(())
    }

    /// The in-frame leaf tail shared by the cache-hit and probe paths: call
    /// `entry` with `(frame_ptr, stack_ptr, ctx)`, check the pending byte (a
    /// throwing leaf slow path bails the whole body), then land the result
    /// on the value stack and jump to `merge`.
    fn emit_leaf_call_tail(
        &mut self,
        entry: ClifValue,
        frame_ptr: ClifValue,
        stack_ptr: ClifValue,
        pre_call_sp: ClifValue,
        merge: Block,
    ) {
        let ctx = self.vm();
        let inst =
            self.builder
                .ins()
                .call_indirect(self.sig_entry, entry, &[frame_ptr, stack_ptr, ctx]);
        let result = self.builder.func.dfg.inst_results(inst)[0];
        let pending =
            self.builder
                .ins()
                .load(types::I8, MemFlagsData::new(), ctx, Offset32::new(0));
        let ok = self.builder.ins().icmp_imm_u(IntCC::Equal, pending, 0);
        let cont = self.builder.create_block();
        let err = self.builder.create_block();
        self.builder.ins().brif(ok, cont, &[], err, &[]);
        self.builder.switch_to_block(err);
        let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
        self.builder.ins().return_(&[undef]);
        self.builder.seal_block(err);
        self.builder.switch_to_block(cont);
        self.builder.def_var(self.sp_var, pre_call_sp);
        self.push(result);
        self.builder.ins().jump(merge, &[]);
    }

    /// Cut 46/47: rebind the frame for a self-tail-call re-entry — params
    /// from the arguments at `[sp - argc*8, sp)`, missing params and the
    /// var/lexical/`this` slots back to their entry state (tdz-aware) — then
    /// jump to the body's re-entry block, which re-seeds the working-stack
    /// base and the per-run variables. Shared by `TailCallSelf` (the static
    /// self-binding form) and `TailCallSelfCheck`'s identity-match path.
    fn emit_self_rebind(&mut self, scope: &ScopeInfo, argc: usize, sp: ClifValue) {
        for slot in 0..scope.frame_size {
            let value = if slot < scope.arity && slot < argc {
                let ptr = self
                    .builder
                    .ins()
                    .iadd_imm_s(sp, -(((argc - slot) as i64) * 8));
                self.builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), ptr, Offset32::new(0))
            } else if scope.tdz_store.get(slot).copied().unwrap_or(false) {
                self.builder.ins().iconst(types::I64, self.uninit_bits)
            } else {
                self.builder.ins().iconst(types::I64, self.undef_bits)
            };
            self.store_slot(slot, value);
        }
        let entry = self
            .reentry_block
            .expect("a self-tail-call body always has a re-entry block");
        self.builder.ins().jump(entry, &[]);
    }

    /// Cut 45: the shared tail-call tail — call the `tail_call` helper with
    /// the callee/this/arg-region and return its result as this body's
    /// completion value. The helper either replaced the current frame on the
    /// Vm (the runtime loops on the new body) or ran the callee as a normal
    /// call; either way the JIT body terminates here.
    fn emit_tail_call(
        &mut self,
        callee: ClifValue,
        this: ClifValue,
        argc: usize,
        args_ptr: ClifValue,
        direct_eval: bool,
    ) -> Result<(), Unsupported> {
        let argc_imm = self.builder.ins().iconst(types::I64, argc as i64);
        let direct_eval_imm = self
            .builder
            .ins()
            .iconst(types::I64, i64::from(direct_eval));
        let result = self.call_slow(
            self.sig_tail,
            Helper::TailCall,
            &[callee, this, argc_imm, args_ptr, direct_eval_imm],
        )?;
        self.builder.ins().return_(&[result]);
        Ok(())
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
            // Cut 62: a heap constant (a string/bigint literal) embeds its
            // NaN-boxed pointer bits directly. Sound because the box never
            // moves (the GC's `Gc` handles are Copy — the weak-table
            // compaction only clears entries) and the value stays alive for
            // the code's lifetime: the step's `Push`/`LoadConst` holds it,
            // the compiled body is traced (the function-site cache, or the
            // active-run tracer while a script body runs), and the cache
            // entry that frees the code also drops the body.
            Ok(self.builder.ins().iconst(types::I64, value.bits() as i64))
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

    /// `bits & TAG_MASK == TAG_PREFIX` and `(bits >> 44) & 0xF == TAG_STRING`
    /// — the string check (I8). The heap-prefix test runs first: a double
    /// can carry the tag bits by coincidence (same discipline as the member
    /// probe's object check).
    fn is_string(&mut self, bits: ClifValue) -> ClifValue {
        let is_heap = self.builder.ins().band_imm_u(bits, crux::TAG_MASK as i64);
        let is_heap = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, is_heap, crux::TAG_PREFIX as i64);
        let tag = self.builder.ins().ushr_imm_u(bits, 44);
        let tag = self.builder.ins().band_imm_u(tag, 0xF);
        let is_str = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, tag, crux::TAG_STRING as i64);
        self.builder.ins().band(is_heap, is_str)
    }

    /// `bits & TAG_MASK == TAG_PREFIX` and `(bits >> 44) & 0xF ==
    /// TAG_FUNCTION` — the function check (M10: the compiled `CallApply`
    /// fast path's callee gate — a non-function receiver must keep the slow
    /// path's exact `do_call_apply` TypeError). Same discipline as
    /// `is_string` (the heap-prefix test first).
    fn is_function(&mut self, bits: ClifValue) -> ClifValue {
        let is_heap = self.builder.ins().band_imm_u(bits, crux::TAG_MASK as i64);
        let is_heap = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, is_heap, crux::TAG_PREFIX as i64);
        let tag = self.builder.ins().ushr_imm_u(bits, 44);
        let tag = self.builder.ins().band_imm_u(tag, 0xF);
        let is_fn = self
            .builder
            .ins()
            .icmp_imm_u(IntCC::Equal, tag, crux::TAG_FUNCTION as i64);
        self.builder.ins().band(is_heap, is_fn)
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
        // Cut 55: the pending-error dispatch and the control helpers pass the
        // interpreter's ip for this step (the loop-top increment means a
        // step's error/transfer is attributed to `index + 1`).
        self.current_step = index;
        // Cut 70: a catch/finally entry step resumes at the handler's
        // try-entry sp (saved at `EnterTry`). A helper error inside the try
        // leaves the erroring step's operands on the working stack — the
        // interpreter's covered error keeps them on its growing Vec (an
        // invisible leak), but the JIT's fixed buffer would overflow as a
        // hot catch/finally loop drifts +operands per iteration. The catch
        // and finally regions never read values from the try body, so
        // resuming at the entry depth is unobservable and bounds the buffer.
        // Skipped for suspension bodies: a resume restores the working
        // region into a FRESH buffer, so a pre-suspension try-entry sp (and
        // every other absolute sp) is stale after the resume.
        if !self.has_suspension
            && let Some(handler) = self.handler_entry_steps.get(&index)
        {
            let entry = self.builder.use_var(self.handler_sp_vars[*handler]);
            self.builder.def_var(self.sp_var, entry);
        }
        match step {
            Step::Push(value) => {
                let bits = match self.const_value(value) {
                    Ok(bits) => bits,
                    // Cut 54: the step-index fallback reads the payload back
                    // from the running body. Since Cut 62 `const_value`
                    // embeds a heap constant's bits directly (a plain
                    // string/bigint literal — `compile_literal` emits
                    // `Push(Value::String(..))`, only templates use
                    // `PushStr`), this arm is a safety net for any exotic
                    // value a `Push` should never carry.
                    Err(_) => {
                        let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                        self.call_slow(self.sig_step, Helper::PushConst, &[step_imm])?
                    }
                };
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
                // The interpreter's `FusedStoreLocal` also sets the statement
                // completion (a statement-position `x = v`); `StoreLocal` does
                // not — its result is popped by a following `SetCompletion`.
                if matches!(step, Step::FusedStoreLocal { .. }) {
                    self.emit_completion_store(value, false);
                }
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
            Step::LoadContextSlot { depth, index: slot } => {
                // `context_chain_env(depth)` slot `slot` through the slow
                // path (the env machinery is shared with the interpreter).
                let depth_imm = self.builder.ins().iconst(types::I64, *depth as i64);
                let slot_imm = self.builder.ins().iconst(types::I64, *slot as i64);
                let res = self.call_slow(
                    self.sig_get_name,
                    Helper::LoadContext,
                    &[depth_imm, slot_imm],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::StoreContextSlot { depth, index: slot } => {
                let value = self.pop();
                let depth_imm = self.builder.ins().iconst(types::I64, *depth as i64);
                let slot_imm = self.builder.ins().iconst(types::I64, *slot as i64);
                let _res = self.call_slow(
                    self.sig_set_name,
                    Helper::StoreContext,
                    &[depth_imm, slot_imm, value],
                )?;
                self.fall_through(index);
            }
            Step::InitContextSlot { index: slot } => {
                let value = self.pop();
                let slot_imm = self.builder.ins().iconst(types::I64, *slot as i64);
                let _res =
                    self.call_slow(self.sig_get_name, Helper::InitContext, &[slot_imm, value])?;
                self.fall_through(index);
            }
            Step::UpdateContextSlot {
                depth,
                index: slot,
                op,
                prefix,
            } => {
                let depth_imm = self.builder.ins().iconst(types::I64, *depth as i64);
                let slot_imm = self.builder.ins().iconst(types::I64, *slot as i64);
                let op_imm = self.builder.ins().iconst(types::I64, *op as i64);
                let prefix_imm = self.builder.ins().iconst(types::I64, *prefix as i64);
                let res = self.call_slow(
                    self.sig_call,
                    Helper::UpdateContext,
                    &[depth_imm, slot_imm, op_imm, prefix_imm],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::LoadPerIteration { depth, index: slot } => {
                // `per_iteration_env(depth)` slot `slot` through the slow
                // path (a captured for-head binding's fresh per-iteration
                // env).
                let depth_imm = self.builder.ins().iconst(types::I64, *depth as i64);
                let slot_imm = self.builder.ins().iconst(types::I64, *slot as i64);
                let res = self.call_slow(
                    self.sig_get_name,
                    Helper::LoadPerIter,
                    &[depth_imm, slot_imm],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::StorePerIteration { depth, index: slot } => {
                let value = self.pop();
                let depth_imm = self.builder.ins().iconst(types::I64, *depth as i64);
                let slot_imm = self.builder.ins().iconst(types::I64, *slot as i64);
                let _res = self.call_slow(
                    self.sig_set_name,
                    Helper::StorePerIter,
                    &[depth_imm, slot_imm, value],
                )?;
                self.fall_through(index);
            }
            Step::UpdatePerIteration {
                depth,
                index: slot,
                op,
                prefix,
            } => {
                let depth_imm = self.builder.ins().iconst(types::I64, *depth as i64);
                let slot_imm = self.builder.ins().iconst(types::I64, *slot as i64);
                let op_imm = self.builder.ins().iconst(types::I64, *op as i64);
                let prefix_imm = self.builder.ins().iconst(types::I64, *prefix as i64);
                let res = self.call_slow(
                    self.sig_call,
                    Helper::UpdatePerIter,
                    &[depth_imm, slot_imm, op_imm, prefix_imm],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::GetVarReference => {
                // The reference stack's top read (no pop — the write path
                // consumes it); the value goes on the JIT stack.
                let res = self.call_slow(self.sig_tdz, Helper::GetVarReference, &[])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::UpdateVarReference { op, prefix } => {
                let old = self.pop();
                let op_imm = self.builder.ins().iconst(types::I64, *op as i64);
                let prefix_imm = self.builder.ins().iconst(types::I64, *prefix as i64);
                let res = self.call_slow(
                    self.sig_set_name,
                    Helper::UpdateVarReference,
                    &[op_imm, prefix_imm, old],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::PutVarReferenceOp { op } => {
                let value = self.pop();
                let old = self.pop();
                let op_imm = self.builder.ins().iconst(types::I64, *op as i64);
                let res = self.call_slow(
                    self.sig_set_name,
                    Helper::PutVarReferenceOp,
                    &[op_imm, old, value],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::PopVarReference => {
                let _res = self.call_slow(self.sig_tdz, Helper::PopVarReference, &[])?;
                self.fall_through(index);
            }
            Step::Jump(target) => {
                let block = self.ensure_block(*target);
                self.builder.ins().jump(block, &[]);
            }
            Step::TypedArrayLengthHoist { target } => {
                // The hoisted-loop guard (gap-close M6 slice 2): pop the
                // receiver, probe its typed-array length; on a probe hit
                // push the length and fall through (the next step stores it
                // in the hidden hoist slot), else jump to the general
                // per-iteration loop (nothing pushed there).
                let object = self.pop();
                let len = self.emit_raw_call(self.sig_bool, Helper::TypedArrayLength, &[object])?;
                let sentinel = self
                    .builder
                    .ins()
                    .iconst(types::I64, TYPED_ARRAY_LENGTH_SENTINEL as i64);
                let hit = self.builder.ins().icmp(IntCC::NotEqual, len, sentinel);
                let hit_block = self.builder.create_block();
                let miss = self.ensure_block(*target);
                self.builder.ins().brif(hit, hit_block, &[], miss, &[]);
                self.builder.switch_to_block(hit_block);
                self.push(len);
                self.fall_through(index);
                self.builder.seal_block(hit_block);
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
            Step::JumpIfRelLimit {
                op,
                slot,
                limit,
                target,
            } => {
                // The fused initial loop test with a fast-binding limit
                // (`i < n` where `n` is a slot/global): runs once per loop,
                // so the relational-slow helper is fine. The counter and the
                // limit slots are TDZ-checked like `LoadLocal`.
                let counter = self.load_slot(*slot);
                if self.slot_is_lexical(*slot) {
                    self.emit_tdz_check(counter)?;
                }
                let limit_bits = match limit {
                    RelLimit::Imm(_) => {
                        unreachable!("the literal limit uses the JumpIf*Imm steps")
                    }
                    RelLimit::Slot(limit_slot) => {
                        let bits = self.load_slot(*limit_slot);
                        if self.slot_is_lexical(*limit_slot) {
                            self.emit_tdz_check(bits)?;
                        }
                        bits
                    }
                    RelLimit::Global(name) => self.emit_global_read(*name)?,
                };
                let op_imm = self.builder.ins().iconst(types::I64, *op as i64);
                let test = self.call_slow(
                    self.sig_rel,
                    Helper::RelationalSlow,
                    &[op_imm, counter, limit_bits],
                )?;
                let block = self.ensure_block(*target);
                self.cond_jump(test, false, block, index + 1);
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
                limit,
                inc,
                body_start,
                after,
            } => self.emit_fast_loop_head(*var, *op, *limit, *inc, *body_start, *after)?,
            Step::RunRegBody { ops } => {
                let entry_sp = self.builder.use_var(self.sp_var);
                let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
                self.builder.def_var(self.acc_var, undef);
                let returns = matches!(ops.last(), Some(LeafOp::ReturnAcc));
                // Cut 55: a helper error inside the run must truncate the
                // transient stack use to the entry depth before the pending
                // dispatch (the interpreter truncates before propagating).
                self.error_sp = Some(entry_sp);
                for (op_index, op) in ops.iter().enumerate() {
                    self.emit_leaf_op(index, op_index, op)?;
                }
                self.error_sp = None;
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
                // Cut 38: inline the member-value fast cell (see
                // `emit_member_cell_read`). Function receivers, primitives
                // (boxing), proxies, accessors, and prototype-chain reads
                // miss to the helper.
                let object = self.pop();
                let value = self.emit_member_cell_read(object, *name)?;
                self.push(value);
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
                // `[object, old?, value]` — the compound forms carry the
                // cached GetValue (the `Dup` + `GetMemberName` the compiler
                // emitted) between the object and the value.
                let value = self.pop();
                let old = if is_compound_assign(op) {
                    Some(self.pop())
                } else {
                    None
                };
                let object = self.pop();
                let op_imm = self.builder.ins().iconst(types::I64, *op as i64);
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let old_imm = match old {
                    Some(bits) => bits,
                    None => self.builder.ins().iconst(types::I64, self.undef_bits),
                };
                // Cut 40: inline the write for a plain-object receiver whose
                // member value cell validates (the property is an own
                // writable data property since the cell was warmed): the
                // compound's new value is computed here when both operands
                // are numbers with an inline op (the f64 op is exact), then
                // `set_member_slot` writes the property vector in place —
                // no `assign_member_name` helper, no [[Set]] chain walk.
                // Non-number compounds, non-inline ops, cold cells, and
                // non-object receivers fall back to the full helper (which
                // applies the op).
                let ctx = self.vm();
                let compound = is_compound_assign(op);
                let inline_compound = compound
                    && matches!(
                        op,
                        AssignOp::AddAssign
                            | AssignOp::SubAssign
                            | AssignOp::MulAssign
                            | AssignOp::DivAssign
                    );
                let is_heap = self.builder.ins().band_imm_u(object, crux::TAG_MASK as i64);
                let is_obj =
                    self.builder
                        .ins()
                        .icmp_imm_u(IntCC::Equal, is_heap, crux::TAG_PREFIX as i64);
                let tag = self.builder.ins().ushr_imm_u(object, 44);
                let tag = self.builder.ins().band_imm_u(tag, 0xF);
                let tag_obj =
                    self.builder
                        .ins()
                        .icmp_imm_u(IntCC::Equal, tag, crux::TAG_OBJECT as i64);
                let obj_ok = self.builder.ins().band(is_obj, tag_obj);
                let gate = if inline_compound {
                    let old_bits = old.expect("compound assigns carry the cached old value");
                    let old_num = self.is_double(old_bits);
                    let value_num = self.is_double(value);
                    let nums = self.builder.ins().band(old_num, value_num);
                    self.builder.ins().band(obj_ok, nums)
                } else if compound {
                    // A compound op the machine code cannot inline
                    // (rem/exp/bitwise/shifts, or non-number operands): the
                    // helper applies it.
                    self.builder.ins().iconst(types::I8, 0)
                } else {
                    obj_ok
                };
                let new_var = self.builder.declare_var(types::I64);
                let fast_prep = self.builder.create_block();
                let slow = self.builder.create_block();
                let merge = self.builder.create_block();
                self.builder.ins().brif(gate, fast_prep, &[], slow, &[]);
                // The inline new-value computation, then the member-cell
                // probe (mirrors `GetMemberName`): the live id/generation
                // must match the cell the interpreter warmed on an own
                // data-property read.
                self.builder.switch_to_block(fast_prep);
                let new = if inline_compound {
                    let old_bits = old.expect("compound assigns carry the cached old value");
                    let old_f =
                        self.builder
                            .ins()
                            .bitcast(types::F64, MemFlagsData::new(), old_bits);
                    let value_f =
                        self.builder
                            .ins()
                            .bitcast(types::F64, MemFlagsData::new(), value);
                    let res = match op {
                        AssignOp::AddAssign => self.builder.ins().fadd(old_f, value_f),
                        AssignOp::SubAssign => self.builder.ins().fsub(old_f, value_f),
                        AssignOp::MulAssign => self.builder.ins().fmul(old_f, value_f),
                        AssignOp::DivAssign => self.builder.ins().fdiv(old_f, value_f),
                        _ => unreachable!("the gate restricted the compound op"),
                    };
                    let res_bits = self
                        .builder
                        .ins()
                        .bitcast(types::I64, MemFlagsData::new(), res);
                    self.canon(res_bits)
                } else {
                    value
                };
                self.builder.def_var(new_var, new);
                let cells = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    ctx,
                    Offset32::new(std::mem::offset_of!(JitCallContext, member_value_cells) as i32),
                );
                let ptr = self
                    .builder
                    .ins()
                    .band_imm_u(object, crux::PAYLOAD_MASK as i64);
                let ptr = self.builder.ins().ishl_imm_u(ptr, 4);
                // The payload stores the `GcBox` base; the `JsObject` sits
                // after the box header (`mark` + `size`).
                let ptr = self
                    .builder
                    .ins()
                    .iadd_imm_s(ptr, crux::heap::GCBOX_DATA_OFFSET as i64);
                let live_id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    ptr,
                    Offset32::new(std::mem::offset_of!(crux::JsObject, id) as i32),
                );
                let live_gen = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    ptr,
                    Offset32::new(std::mem::offset_of!(crux::JsObject, generation) as i32),
                );
                let cell_slot = self.builder.ins().bxor(live_id, name_imm);
                let cell_slot = self
                    .builder
                    .ins()
                    .band_imm_u(cell_slot, (MEMBER_CELLS - 1) as i64);
                let index_bytes = self
                    .builder
                    .ins()
                    .imul_imm_s(cell_slot, std::mem::size_of::<MemberValueCell>() as i64);
                let cell = self.builder.ins().iadd(cells, index_bytes);
                let cell_id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(MemberValueCell, id) as i32),
                );
                let cell_name = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(MemberValueCell, name) as i32),
                );
                let cell_gen = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(MemberValueCell, generation) as i32),
                );
                let id_ok = self.builder.ins().icmp(IntCC::Equal, cell_id, live_id);
                let name_ok = self
                    .builder
                    .ins()
                    .icmp_imm_u(IntCC::Equal, cell_name, *name as i64);
                let gen_ok = self.builder.ins().icmp(IntCC::Equal, cell_gen, live_gen);
                let id_name_ok = self.builder.ins().band(id_ok, name_ok);
                let ok = self.builder.ins().band(id_name_ok, gen_ok);
                let fast = self.builder.create_block();
                self.builder.ins().brif(ok, fast, &[], slow, &[]);
                // The fast path: the helper writes the property vector in
                // place and refreshes the cell (the write does not bump the
                // generation, so the next read probe stays warm). The result
                // is the stored value.
                self.builder.switch_to_block(fast);
                let new = self.builder.use_var(new_var);
                let stored = self.call_slow(
                    self.sig_set_name,
                    Helper::SetMemberSlot,
                    &[object, name_imm, new],
                )?;
                self.push(stored);
                self.builder.ins().jump(merge, &[]);
                // The slow path: the full helper (applies the op with the
                // general machinery).
                self.builder.switch_to_block(slow);
                let res = self.call_slow(
                    self.sig_assign,
                    Helper::AssignMemberName,
                    &[op_imm, object, name_imm, old_imm, value],
                )?;
                self.push(res);
                self.builder.ins().jump(merge, &[]);
                self.builder.seal_block(merge);
                self.builder.switch_to_block(merge);
                self.fall_through(index);
            }
            Step::AssignMemberComputed { op } => {
                // `[object, key, old?, value]`.
                let value = self.pop();
                let old = if is_compound_assign(op) {
                    Some(self.pop())
                } else {
                    None
                };
                let key = self.pop();
                let object = self.pop();
                let op_imm = self.builder.ins().iconst(types::I64, *op as i64);
                let old_imm = match old {
                    Some(bits) => bits,
                    None => self.builder.ins().iconst(types::I64, self.undef_bits),
                };
                if !is_compound_assign(op) {
                    // The inline dense-array append (gap-close M1 C): gate
                    // on the receiver being a dense Array (`array_dense`),
                    // the key a canonical index Number equal to
                    // `slots.length`, then run the stateful append through
                    // the narrow `dense_array_append` helper (extensibility,
                    // chain-clean, push, length + mirror, generation). The
                    // in-place-update / hole-fill / spill shapes and the
                    // typed-array stores fall through to the inline typed
                    // gate / `fast_array_element_write`, and anything else to
                    // `assign_member_computed` — the fallback re-runs the
                    // checks and the [[Set]] machinery correctly (nothing
                    // observable ran on the fast paths).
                    let ta_gate = self.builder.create_block();
                    let fast = self.builder.create_block();
                    let fallback = self.builder.create_block();
                    let merge = self.builder.create_block();
                    self.emit_dense_array_append_inline(object, key, value, ta_gate, merge)?;
                    // The machine typed-array store (gap-close M5c): a fixed
                    // Uint8Array element store with a byte-valued Number key
                    // + value writes the byte straight into the block; the
                    // other typed-array shapes fall to the legacy helper.
                    self.builder.switch_to_block(ta_gate);
                    self.emit_typed_array_store_inline(object, key, value, fast, merge)?;
                    // The legacy helper: typed arrays, in-place updates,
                    // hole-fills, spilled/non-dense arrays, and non-canonical
                    // keys (the pre-existing inline element-store fast path).
                    self.builder.switch_to_block(fast);
                    let ok = self.emit_raw_call(
                        self.sig_binary,
                        Helper::FastArrayElementWrite,
                        &[object, key, value],
                    )?;
                    let hit = self.builder.ins().icmp_imm_u(IntCC::Equal, ok, 1);
                    self.builder.ins().brif(hit, merge, &[], fallback, &[]);
                    self.builder.seal_block(fast);
                    self.builder.switch_to_block(fallback);
                    self.call_slow(
                        self.sig_assign,
                        Helper::AssignMemberComputed,
                        &[op_imm, object, key, old_imm, value],
                    )?;
                    // The slow path's stored value is the RHS for a plain
                    // `=` — the merge below pushes `value` for ALL paths, so
                    // the fallback must not push its result too (an extra
                    // slot would leave the working stack one deeper than
                    // `max_stack_usage` accounts and overrun the buffer).
                    self.builder.ins().jump(merge, &[]);
                    self.builder.seal_block(fallback);
                    self.builder.switch_to_block(merge);
                    self.push(value);
                    self.fall_through(index);
                } else {
                    let res = self.call_slow(
                        self.sig_assign,
                        Helper::AssignMemberComputed,
                        &[op_imm, object, key, old_imm, value],
                    )?;
                    self.push(res);
                    self.fall_through(index);
                }
            }
            Step::CallFast { argc, direct_eval } => {
                // `[..., this, callee, a1..aN]` on the JIT stack; the probe
                // reads the callee by address, and the in-frame leaf path
                // (when the callee is an inlineable leaf) replaces the whole
                // region with the result. The slow path passes the argument
                // region by pointer (the helper copies it out before running
                // the interpreter's call machinery). `direct_eval` (Cut 62:
                // the compiler never emits a true flag — a direct eval
                // always takes the vector form — but the slow path routes
                // it correctly if one appears).
                let sp = self.builder.use_var(self.sp_var);
                let args_ptr = self.builder.ins().iadd_imm_s(sp, -((*argc as i64) * 8));
                let callee_ptr = self.builder.ins().iadd_imm_s(sp, -((*argc as i64 + 1) * 8));
                let this_ptr = self.builder.ins().iadd_imm_s(sp, -((*argc as i64 + 2) * 8));
                let callee = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    callee_ptr,
                    Offset32::new(0),
                );
                let this = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    this_ptr,
                    Offset32::new(0),
                );
                let argc_imm = self.builder.ins().iconst(types::I64, i64::from(*argc));
                self.emit_call(
                    index,
                    callee,
                    this,
                    args_ptr,
                    argc_imm,
                    this_ptr,
                    *direct_eval,
                    true,
                )?;
            }
            Step::CallApply { argc, kind } => {
                // M10: `[..., f, apply/call, thisArg, a1..aN]` — the `CallFast`
                // layout with `argc` = N+1. When the member read resolved the
                // realm's intrinsic `apply`/`call` (compared against the ctx's
                // per-run snapshot) and the receiver `f` is a Function, the
                // machine code rebuilds the direct-call layout and routes into
                // the `emit_call` leaf machinery — skipping the `call_apply`
                // slow-path round trip, the vm.stack rebuild, and the
                // helper's re-checks. Everything else falls back to the
                // `call_apply` helper (the interpreter's `do_call_apply` — a
                // shadowed `apply`/`call` or a non-function receiver keeps
                // its exact behavior).
                let argc_usize = *argc as usize;
                let sp = self.builder.use_var(self.sp_var);
                let args_ptr = self
                    .builder
                    .ins()
                    .iadd_imm_s(sp, -((argc_usize as i64) * 8));
                let resolved_ptr = self
                    .builder
                    .ins()
                    .iadd_imm_s(sp, -(((argc_usize + 1) as i64) * 8));
                let callee_ptr = self
                    .builder
                    .ins()
                    .iadd_imm_s(sp, -(((argc_usize + 2) as i64) * 8));
                let resolved = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    resolved_ptr,
                    Offset32::new(0),
                );
                let callee = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    callee_ptr,
                    Offset32::new(0),
                );
                let pre_call_sp = callee_ptr;
                let ctx = self.vm();
                let intrinsic_offset = match kind {
                    ApplyKind::Apply => {
                        std::mem::offset_of!(JitCallContext, apply_builtin_bits)
                    }
                    ApplyKind::Call => std::mem::offset_of!(JitCallContext, call_builtin_bits),
                };
                let intrinsic = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    ctx,
                    Offset32::new(intrinsic_offset as i32),
                );
                let is_intrinsic = self.builder.ins().icmp(IntCC::Equal, resolved, intrinsic);
                let is_fn = self.is_function(callee);
                let gate = self.builder.ins().band(is_intrinsic, is_fn);
                let fast = self.builder.create_block();
                let slow = self.builder.create_block();
                self.builder.ins().brif(gate, fast, &[], slow, &[]);
                // The fast path: `this` for the direct call is the
                // `thisArg` (args[0]; `undefined` when the site has no
                // arguments — `do_call_apply` reads the default).
                self.builder.switch_to_block(fast);
                let this = if argc_usize == 0 {
                    self.builder.ins().iconst(types::I64, self.undef_bits)
                } else {
                    self.builder.ins().load(
                        types::I64,
                        MemFlagsData::new(),
                        args_ptr,
                        Offset32::new(0),
                    )
                };
                match kind {
                    ApplyKind::Call => {
                        // `f.call(thisArg, a1..aN)`: `this` is args[0], the
                        // direct call's arguments are a1..aN, already
                        // contiguous above it — no copy, just the region
                        // shift.
                        if argc_usize == 0 {
                            let zero = self.builder.ins().iconst(types::I64, 0);
                            self.emit_call(
                                index,
                                callee,
                                this,
                                sp,
                                zero,
                                pre_call_sp,
                                false,
                                true,
                            )?;
                        } else {
                            let args_ptr_new = self.builder.ins().iadd_imm_s(args_ptr, 8);
                            let argc_new = self
                                .builder
                                .ins()
                                .iconst(types::I64, (argc_usize - 1) as i64);
                            self.emit_call(
                                index,
                                callee,
                                this,
                                args_ptr_new,
                                argc_new,
                                pre_call_sp,
                                false,
                                true,
                            )?;
                        }
                    }
                    ApplyKind::Apply => {
                        // `f.apply(thisArg, argArray)`: the direct arguments
                        // come from argArray (args[1]; `undefined` when the
                        // site has fewer than two). A nullish argArray is an
                        // empty argument list; a dense Array's elements are
                        // copied to the buffer top by the fill helper (the
                        // one heap read the machine code cannot do inline);
                        // any other shape falls back to the slow path (no
                        // writes on the reject).
                        let arg_array = if argc_usize >= 2 {
                            let ptr = self.builder.ins().iadd_imm_s(args_ptr, 8);
                            self.builder.ins().load(
                                types::I64,
                                MemFlagsData::new(),
                                ptr,
                                Offset32::new(0),
                            )
                        } else {
                            self.builder.ins().iconst(types::I64, self.undef_bits)
                        };
                        let nullish = self.emit_nullish(arg_array);
                        let zero = self.builder.create_block();
                        let fill = self.builder.create_block();
                        self.builder.ins().brif(nullish, zero, &[], fill, &[]);
                        self.builder.switch_to_block(zero);
                        let zero_argc = self.builder.ins().iconst(types::I64, 0);
                        self.emit_call(
                            index,
                            callee,
                            this,
                            sp,
                            zero_argc,
                            pre_call_sp,
                            false,
                            true,
                        )?;
                        self.builder.switch_to_block(fill);
                        let count = self.call_slow(
                            self.sig_apply_args,
                            Helper::ApplyArgsFill,
                            &[arg_array, sp],
                        )?;
                        let not_fast =
                            self.builder
                                .ins()
                                .icmp_imm_u(IntCC::Equal, count, u64::MAX as i64);
                        let filled = self.builder.create_block();
                        self.builder.ins().brif(not_fast, slow, &[], filled, &[]);
                        self.builder.seal_block(fill);
                        self.builder.switch_to_block(filled);
                        self.emit_call(index, callee, this, sp, count, pre_call_sp, false, true)?;
                    }
                }
                // The slow path: the `call_apply` helper with the original
                // region (the fast path never wrote it — the fill rejects
                // before any write).
                self.builder.switch_to_block(slow);
                let argc_imm = self.builder.ins().iconst(types::I64, i64::from(*argc));
                let kind_imm = self.builder.ins().iconst(
                    types::I64,
                    match kind {
                        ApplyKind::Apply => 0,
                        ApplyKind::Call => 1,
                    },
                );
                let result = self.call_slow(
                    self.sig_call_apply,
                    Helper::CallApply,
                    &[resolved, callee, argc_imm, args_ptr, kind_imm],
                )?;
                self.builder.def_var(self.sp_var, pre_call_sp);
                self.push(result);
                self.fall_through(index);
                self.builder.seal_block(slow);
            }
            Step::CallFastSlot { slot, argc } => {
                // `[..., a1..aN]` — the fused slot call (`do_call_fast_slot`
                // reads the callee from the frame and passes `undefined` as
                // `this`; the fuse guards rule out an argument that writes
                // the slot). A leaf callee at the slot runs in-frame.
                let sp = self.builder.use_var(self.sp_var);
                let args_ptr = self.builder.ins().iadd_imm_s(sp, -((*argc as i64) * 8));
                let callee = self.load_slot(*slot);
                let this = self.builder.ins().iconst(types::I64, self.undef_bits);
                let argc_imm = self.builder.ins().iconst(types::I64, i64::from(*argc));
                self.emit_call(
                    index, callee, this, args_ptr, argc_imm, args_ptr, false, true,
                )?;
            }
            Step::CallFastGlobal {
                name,
                argc,
                direct_eval,
            } => {
                // Cut 65: `[..., a1..aN]` — the fused global call
                // (`do_call_fast_global` reads the callee from the global
                // fast cell and passes `undefined` as `this`; the fuse
                // guarantees the never-assigned global is stable). The
                // machine-code cell read (`emit_global_read`) feeds the
                // same leaf-probe machinery as the stack/slot forms.
                let sp = self.builder.use_var(self.sp_var);
                let args_ptr = self.builder.ins().iadd_imm_s(sp, -((*argc as i64) * 8));
                let callee = self.emit_global_read(*name)?;
                let this = self.builder.ins().iconst(types::I64, self.undef_bits);
                let argc_imm = self.builder.ins().iconst(types::I64, i64::from(*argc));
                self.emit_call(
                    index,
                    callee,
                    this,
                    args_ptr,
                    argc_imm,
                    args_ptr,
                    *direct_eval,
                    true,
                )?;
            }
            Step::CallFastSlotStore {
                arg_slots,
                store_slot,
                ..
            }
            | Step::CallFastGlobalStore {
                arg_slots,
                store_slot,
                ..
            } => {
                // Cut 65: the fused `x = f(args)` — materialize the arg
                // slots onto the working stack (TDZ-checked in order, the
                // `LoadLocal` semantics), run the slot/global-callee call
                // over the region, and store the result to the target (the
                // `FusedStoreLocal` TDZ check). The global form reads the
                // callee from the fast cell; the slot form from the frame.
                for &slot in arg_slots {
                    let bits = self.load_slot(slot);
                    if self.slot_is_lexical(slot) {
                        self.emit_tdz_check(bits)?;
                    }
                    self.push(bits);
                }
                let sp = self.builder.use_var(self.sp_var);
                let args_ptr = self
                    .builder
                    .ins()
                    .iadd_imm_s(sp, -((arg_slots.len() as i64) * 8));
                let callee = match step {
                    Step::CallFastSlotStore { callee_slot, .. } => self.load_slot(*callee_slot),
                    Step::CallFastGlobalStore { name, .. } => self.emit_global_read(*name)?,
                    _ => unreachable!("the or-pattern matched a fused store"),
                };
                let this = self.builder.ins().iconst(types::I64, self.undef_bits);
                let argc_imm = self
                    .builder
                    .ins()
                    .iconst(types::I64, arg_slots.len() as i64);
                self.emit_call(
                    index, callee, this, args_ptr, argc_imm, args_ptr, false, false,
                )?;
                let result = self.pop();
                if self.slot_is_lexical(*store_slot) {
                    let current = self.load_slot(*store_slot);
                    self.emit_tdz_check(current)?;
                }
                self.store_slot(*store_slot, result);
                self.fall_through(index);
            }
            // Cut 49: the vector call form (≥3 args or a spread). The
            // arguments build in `Vm::args` through the helpers (the same
            // channel the interpreter's vector `Call`/`TailCall` handlers
            // read), so the work stack holds only `[this, callee]` at the
            // call step. `ArgsPush`/`ArgsSpread` consume their value.
            Step::ArgsBase => {
                self.call_slow(self.sig_tdz, Helper::ArgsBase, &[])?;
                self.fall_through(index);
            }
            Step::ArgsPush => {
                let value = self.pop();
                self.call_slow(self.sig_bool, Helper::ArgsPush, &[value])?;
                self.fall_through(index);
            }
            Step::ArgsSpread => {
                let iterable = self.pop();
                self.call_slow(self.sig_bool, Helper::ArgsSpread, &[iterable])?;
                self.fall_through(index);
            }
            Step::Call { direct_eval } => {
                // The vector `Call`: `[this, callee]` on the work stack, the
                // arguments in `Vm::args`. The helper bridges both onto
                // `vm.stack` and runs the interpreter's vector `do_call`.
                let callee = self.pop();
                let this = self.pop();
                let direct_eval_imm = self
                    .builder
                    .ins()
                    .iconst(types::I64, i64::from(*direct_eval));
                let result = self.call_slow(
                    self.sig_set_name,
                    Helper::CallVector,
                    &[this, callee, direct_eval_imm],
                )?;
                self.push(result);
                self.fall_through(index);
            }
            // Proper tail calls (Cut 45): the machine code hands the
            // callee/args to the `tail_call` helper and returns its result —
            // an ordinary certified callee replaces the current frame on the
            // Vm (the runtime loops on the new body), anything else is a
            // normal call whose result completes this body's return. The
            // JIT body always terminates here, so no `fall_through`.
            Step::TailCall { direct_eval } => {
                // Cut 49: the vector form — `[this, callee]` on the work
                // stack, the arguments in `Vm::args`. The helper mirrors
                // `tail_call` reading them from the vector.
                let callee = self.pop();
                let this = self.pop();
                let direct_eval_imm = self
                    .builder
                    .ins()
                    .iconst(types::I64, i64::from(*direct_eval));
                let result = self.call_slow(
                    self.sig_set_name,
                    Helper::TailCallVector,
                    &[this, callee, direct_eval_imm],
                )?;
                self.builder.ins().return_(&[result]);
            }
            Step::TailCallFast { argc, direct_eval } => {
                // `[..., this, callee, a1..aN]`.
                let sp = self.builder.use_var(self.sp_var);
                let args_ptr = self.builder.ins().iadd_imm_s(sp, -((*argc as i64) * 8));
                let callee_ptr = self.builder.ins().iadd_imm_s(sp, -((*argc as i64 + 1) * 8));
                let this_ptr = self.builder.ins().iadd_imm_s(sp, -((*argc as i64 + 2) * 8));
                let callee = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    callee_ptr,
                    Offset32::new(0),
                );
                let this = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    this_ptr,
                    Offset32::new(0),
                );
                self.emit_tail_call(callee, this, *argc as usize, args_ptr, *direct_eval)?;
            }
            Step::TailCallFastSlot { slot, argc } => {
                // `[..., a1..aN]`; the callee comes from the frame slot with
                // an `undefined` receiver.
                let sp = self.builder.use_var(self.sp_var);
                let args_ptr = self.builder.ins().iadd_imm_s(sp, -((*argc as i64) * 8));
                let callee = self.load_slot(*slot);
                let this = self.builder.ins().iconst(types::I64, self.undef_bits);
                self.emit_tail_call(callee, this, *argc as usize, args_ptr, false)?;
            }
            Step::TailCallFastGlobal { name, argc } => {
                // `[..., a1..aN]`; the callee comes from the global fast cell
                // (the `LoadGlobal` inline read) with an `undefined` receiver.
                let sp = self.builder.use_var(self.sp_var);
                let args_ptr = self.builder.ins().iadd_imm_s(sp, -((*argc as i64) * 8));
                let callee = self.emit_global_read(*name)?;
                let this = self.builder.ins().iconst(types::I64, self.undef_bits);
                self.emit_tail_call(callee, this, *argc as usize, args_ptr, false)?;
            }
            Step::TailCallSelf { argc } => {
                // Cut 46: the callee is the named-function-expression
                // self-binding — statically the running body itself. The
                // arguments sit alone at `[sp - argc*8, sp)` (the compiler
                // skipped the this/callee pushes). Rebind the frame in place
                // exactly like the runtime's fresh-run setup (params from the
                // arguments, missing params and the var/lexical/`this` slots
                // back to their entry state), reset the working stack to the
                // entry base, and jump back to the body's entry — the whole
                // self-recursive tail chain runs in ONE machine-code
                // invocation, no runtime round-trip. The Vm state the machine
                // code and its helpers leave between steps is exactly the
                // entry state for a certified capture-free body (every helper
                // restores the shared stacks; the body has no env/iterator
                // machinery), so the re-entry needs only the frame rebind and
                // the working-stack reset.
                let scope = self
                    .scope
                    .ok_or(Unsupported::Step("TailCallSelf without a certified scope"))?;
                let argc = *argc as usize;
                let sp = self.builder.use_var(self.sp_var);
                self.emit_self_rebind(scope, argc, sp);
            }
            Step::TailCallSelfCheck { argc } => {
                // Cut 47: a tail call to the enclosing function's own name in
                // a body that is NOT a named expression (a declaration — the
                // name resolves through the global/outer env and could have
                // been reassigned). The callee/this/args sit on the stack as
                // for `TailCallFast`; the machine code compares the resolved
                // callee against the running closure (`ctx.current_function`,
                // set at entry from the Vm's `current_function` and exact —
                // the bits match iff the callee IS the running body). On a
                // match the frame is rebound and the body re-enters its start
                // (the whole self-recursive chain in one invocation);
                // otherwise the general `tail_call` helper runs.
                let scope = self.scope.ok_or(Unsupported::Step(
                    "TailCallSelfCheck without a certified scope",
                ))?;
                let argc = *argc as usize;
                let sp = self.builder.use_var(self.sp_var);
                let args_ptr = self.builder.ins().iadd_imm_s(sp, -((argc as i64) * 8));
                let callee_ptr = self
                    .builder
                    .ins()
                    .iadd_imm_s(sp, -(((argc + 1) as i64) * 8));
                let this_ptr = self
                    .builder
                    .ins()
                    .iadd_imm_s(sp, -(((argc + 2) as i64) * 8));
                let callee = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    callee_ptr,
                    Offset32::new(0),
                );
                let this = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    this_ptr,
                    Offset32::new(0),
                );
                let ctx = self.vm();
                let current = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    ctx,
                    Offset32::new(std::mem::offset_of!(JitCallContext, current_function) as i32),
                );
                let is_self = self.builder.ins().icmp(IntCC::Equal, callee, current);
                let self_block = self.builder.create_block();
                let helper_block = self.builder.create_block();
                self.builder
                    .ins()
                    .brif(is_self, self_block, &[], helper_block, &[]);
                // The self path: rebind the frame with the new arguments and
                // re-enter the body's start.
                self.builder.switch_to_block(self_block);
                self.emit_self_rebind(scope, argc, sp);
                self.builder.seal_block(self_block);
                // The general path: the `tail_call` helper with the resolved
                // callee (a reassigned name, or a cross-body call).
                self.builder.switch_to_block(helper_block);
                self.emit_tail_call(callee, this, argc, args_ptr, false)?;
                self.builder.seal_block(helper_block);
            }
            Step::TailCallSelfVector => {
                // Cut 51: the vector-form self-tail-call — the arguments sit
                // in the Vm's vector (the vector-build steps consumed their
                // operands), no receiver/callee on the work stack. The
                // helper pops the boundary, rebinds the frame in place from
                // the argument vector, and returns 1; the machine code then
                // jumps back to the body's re-entry block — the same
                // single-invocation self-chain as the fast-form jump. On a
                // helper error (0) the block terminates instead: the pending
                // error surfaces when the JIT run returns, and the re-entry
                // must NOT run again.
                let ok = self.call_slow(self.sig_tdz, Helper::TailCallSelfVector, &[])?;
                let entry = self.reentry_block.ok_or(Unsupported::Step(
                    "TailCallSelfVector without a re-entry block",
                ))?;
                let ok_block = self.builder.create_block();
                let err_block = self.builder.create_block();
                let is_ok = self.builder.ins().icmp_imm_u(IntCC::NotEqual, ok, 0);
                self.builder
                    .ins()
                    .brif(is_ok, ok_block, &[], err_block, &[]);
                self.builder.switch_to_block(ok_block);
                self.builder.ins().jump(entry, &[]);
                self.builder.seal_block(ok_block);
                self.builder.switch_to_block(err_block);
                self.builder.ins().return_(&[ok]);
                self.builder.seal_block(err_block);
            }
            Step::TailCallSelfCheckVector => {
                // Cut 51: the vector-form checked self-tail-call — `[this,
                // callee]` on the work stack, the arguments in the Vm's
                // vector. The machine code compares the resolved callee
                // against the running closure (`ctx.current_function`); on a
                // match the frame is rebound from the vector and the body
                // re-enters its start (the whole chain in one invocation);
                // otherwise the general vector `tail_call` helper runs.
                let sp = self.builder.use_var(self.sp_var);
                let callee_ptr = self.builder.ins().iadd_imm_s(sp, -8);
                let this_ptr = self.builder.ins().iadd_imm_s(sp, -16);
                let callee = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    callee_ptr,
                    Offset32::new(0),
                );
                let this = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    this_ptr,
                    Offset32::new(0),
                );
                let ctx = self.vm();
                let current = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    ctx,
                    Offset32::new(std::mem::offset_of!(JitCallContext, current_function) as i32),
                );
                let is_self = self.builder.ins().icmp(IntCC::Equal, callee, current);
                let self_block = self.builder.create_block();
                let helper_block = self.builder.create_block();
                self.builder
                    .ins()
                    .brif(is_self, self_block, &[], helper_block, &[]);
                // The self path: rebind the frame from the argument vector
                // and re-enter the body's start.
                self.builder.switch_to_block(self_block);
                let ok = self.call_slow(self.sig_tdz, Helper::TailCallSelfVector, &[])?;
                let entry = self.reentry_block.ok_or(Unsupported::Step(
                    "TailCallSelfCheckVector without a re-entry block",
                ))?;
                let ok_block = self.builder.create_block();
                let err_block = self.builder.create_block();
                let is_ok = self.builder.ins().icmp_imm_u(IntCC::NotEqual, ok, 0);
                self.builder
                    .ins()
                    .brif(is_ok, ok_block, &[], err_block, &[]);
                self.builder.switch_to_block(ok_block);
                self.builder.ins().jump(entry, &[]);
                self.builder.seal_block(ok_block);
                self.builder.switch_to_block(err_block);
                self.builder.ins().return_(&[ok]);
                self.builder.seal_block(err_block);
                self.builder.seal_block(self_block);
                // The general path: the vector `tail_call` helper with the
                // resolved callee (a reassigned name, or a cross-body call).
                self.builder.switch_to_block(helper_block);
                let zero = self.builder.ins().iconst(types::I64, 0);
                let result = self.call_slow(
                    self.sig_set_name,
                    Helper::TailCallVector,
                    &[this, callee, zero],
                )?;
                self.builder.ins().return_(&[result]);
                self.builder.seal_block(helper_block);
            }
            Step::LoadGlobal { name } => {
                let value = self.emit_global_read(*name)?;
                self.push(value);
                self.fall_through(index);
            }
            Step::StoreGlobal { name } | Step::FusedStoreGlobal { name } => {
                // Cut 36: inline the direct-mapped global-value fast cell on
                // the write side too. The compiled code validates the cell
                // (name + the global's LIVE id/generation + a resolved
                // slot), updates the cell's cached value (keeping the
                // compiled `LoadGlobal` fast path warm — a plain cached
                // write does not bump the generation), and hands the
                // property-vector write to `set_global_slot` with the
                // validated slot — the one part that cannot be inlined (the
                // vector's enum layout is runtime internal). A stale cell,
                // an unknown slot, or a null global falls through to
                // `set_global`, which re-resolves and mirrors the cell for
                // the next read.
                let value = self.pop();
                let ctx = self.vm();
                let global = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    ctx,
                    Offset32::new(std::mem::offset_of!(JitCallContext, global_object) as i32),
                );
                let cells = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    ctx,
                    Offset32::new(std::mem::offset_of!(JitCallContext, global_value_cells) as i32),
                );
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let probe = self.builder.create_block();
                let fast = self.builder.create_block();
                let slow = self.builder.create_block();
                let merge = self.builder.create_block();
                let has_global = self.builder.ins().icmp_imm_u(IntCC::NotEqual, global, 0);
                let has_global_64 = self.bint(has_global);
                self.builder
                    .ins()
                    .brif(has_global_64, probe, &[], slow, &[]);
                self.builder.switch_to_block(probe);
                let live_id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    global,
                    Offset32::new(std::mem::offset_of!(crux::JsObject, id) as i32),
                );
                let live_gen = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    global,
                    Offset32::new(std::mem::offset_of!(crux::JsObject, generation) as i32),
                );
                let cell = self.builder.ins().iadd_imm_s(
                    cells,
                    ((*name as usize & (GLOBAL_CELLS - 1)) * std::mem::size_of::<GlobalValueCell>())
                        as i64,
                );
                let cell_name = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(GlobalValueCell, name) as i32),
                );
                let cell_id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(GlobalValueCell, global_id) as i32),
                );
                let cell_gen = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(GlobalValueCell, generation) as i32),
                );
                let cell_slot = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(GlobalValueCell, slot) as i32),
                );
                let name_ok = self
                    .builder
                    .ins()
                    .icmp_imm_u(IntCC::Equal, cell_name, *name as i64);
                let id_ok = self.builder.ins().icmp(IntCC::Equal, cell_id, live_id);
                let gen_ok = self.builder.ins().icmp(IntCC::Equal, cell_gen, live_gen);
                // A load-only cell (its slot never resolved) stays valid for
                // reads but cannot serve the store's vector write.
                let cell_slot_64 = self.builder.ins().uextend(types::I64, cell_slot);
                let slot_ok =
                    self.builder
                        .ins()
                        .icmp_imm_u(IntCC::NotEqual, cell_slot_64, u32::MAX as i64);
                let name_id_ok = self.builder.ins().band(name_ok, id_ok);
                let name_id_gen_ok = self.builder.ins().band(name_id_ok, gen_ok);
                let ok = self.builder.ins().band(name_id_gen_ok, slot_ok);
                self.builder.ins().brif(ok, fast, &[], slow, &[]);
                // The fast path: update the cached value, then write the
                // property vector through the validated slot.
                self.builder.switch_to_block(fast);
                self.builder.ins().store(
                    MemFlagsData::new(),
                    value,
                    cell,
                    Offset32::new(std::mem::offset_of!(GlobalValueCell, value) as i32),
                );
                let _stored = self.call_slow(
                    self.sig_set_name,
                    Helper::SetGlobalSlot,
                    &[name_imm, cell_slot_64, value],
                )?;
                self.builder.ins().jump(merge, &[]);
                // The slow path: re-resolve through the runtime (which also
                // mirrors the cell for the next read).
                self.builder.switch_to_block(slow);
                let _stored =
                    self.call_slow(self.sig_update, Helper::SetGlobal, &[name_imm, value])?;
                self.builder.ins().jump(merge, &[]);
                self.builder.seal_block(merge);
                self.builder.switch_to_block(merge);
                // The interpreter's `FusedStoreGlobal` also sets the statement
                // completion (a statement-position `x = v` on a declared
                // global); `StoreGlobal` does not — its result is popped by a
                // following `SetCompletion`.
                if matches!(step, Step::FusedStoreGlobal { .. }) {
                    self.emit_completion_store(value, false);
                }
                self.fall_through(index);
            }
            Step::LoadIdent { name } => {
                // Cut 36: the certified body's global read (`BindingLoc::Env`
                // — a name the scope analysis proved resolves at the global
                // env). The compiled code probes the same direct-mapped
                // global-value cell as `LoadGlobal`, gated on the ctx's
                // `clean_chain` (the probe is sound only when the body's env
                // chain is exactly the global env — any intermediate env
                // could shadow a name the cell records), and falls back to
                // the full `load_ident` resolve on a miss. `load_ident`
                // warms the cell when it lands on a global object-record
                // data property, so the second read of a hot loop hits the
                // native load.
                let ctx = self.vm();
                let clean = self.builder.ins().load(
                    types::I8,
                    MemFlagsData::new(),
                    ctx,
                    Offset32::new(std::mem::offset_of!(JitCallContext, clean_chain) as i32),
                );
                let global = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    ctx,
                    Offset32::new(std::mem::offset_of!(JitCallContext, global_object) as i32),
                );
                let cells = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    ctx,
                    Offset32::new(std::mem::offset_of!(JitCallContext, global_value_cells) as i32),
                );
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let value_var = self.builder.declare_var(types::I64);
                let slow = self.builder.create_block();
                let merge = self.builder.create_block();
                let has_global = self.builder.ins().icmp_imm_u(IntCC::NotEqual, global, 0);
                let clean_ok = self.builder.ins().icmp_imm_u(IntCC::NotEqual, clean, 0);
                let has_global_64 = self.bint(has_global);
                let clean_ok_64 = self.bint(clean_ok);
                let gate = self.builder.ins().band(has_global_64, clean_ok_64);
                let probe = self.builder.create_block();
                self.builder.ins().brif(gate, probe, &[], slow, &[]);
                // The fast path: the cell's name and captured version must
                // match the live global (a stale cell, another realm's
                // global, or a mid-run mutation all miss).
                self.builder.switch_to_block(probe);
                let live_id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    global,
                    Offset32::new(std::mem::offset_of!(crux::JsObject, id) as i32),
                );
                let live_gen = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    global,
                    Offset32::new(std::mem::offset_of!(crux::JsObject, generation) as i32),
                );
                let cell = self.builder.ins().iadd_imm_s(
                    cells,
                    ((*name as usize & (GLOBAL_CELLS - 1)) * std::mem::size_of::<GlobalValueCell>())
                        as i64,
                );
                let cell_name = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(GlobalValueCell, name) as i32),
                );
                let cell_id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(GlobalValueCell, global_id) as i32),
                );
                let cell_gen = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(GlobalValueCell, generation) as i32),
                );
                let cell_value = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(GlobalValueCell, value) as i32),
                );
                let name_ok = self
                    .builder
                    .ins()
                    .icmp_imm_u(IntCC::Equal, cell_name, *name as i64);
                let id_ok = self.builder.ins().icmp(IntCC::Equal, cell_id, live_id);
                let gen_ok = self.builder.ins().icmp(IntCC::Equal, cell_gen, live_gen);
                let name_id_ok = self.builder.ins().band(name_ok, id_ok);
                let ok = self.builder.ins().band(name_id_ok, gen_ok);
                self.builder.def_var(value_var, cell_value);
                self.builder.ins().brif(ok, merge, &[], slow, &[]);
                // The slow path: the full resolve (which also warms the cell
                // for the next read when the binding is a global data
                // property).
                self.builder.switch_to_block(slow);
                let res = self.call_slow(self.sig_bool, Helper::LoadIdent, &[name_imm])?;
                self.builder.def_var(value_var, res);
                self.builder.ins().jump(merge, &[]);
                self.builder.seal_block(merge);
                self.builder.switch_to_block(merge);
                let value = self.builder.use_var(value_var);
                self.push(value);
                self.fall_through(index);
            }
            Step::ResolveVarIdent { name } => {
                // The reference goes on the Vm's reference stack (the write
                // path's `put_var_reference` pops it); no JIT stack effect.
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let _res = self.call_slow(self.sig_bool, Helper::ResolveVarIdent, &[name_imm])?;
                self.fall_through(index);
            }
            Step::PutVarReference => {
                // The assignment's value is re-pushed after the store (the
                // interpreter's handler pushes it back).
                let value = self.pop();
                let stored = self.call_slow(self.sig_bool, Helper::PutVarReference, &[value])?;
                self.push(stored);
                self.fall_through(index);
            }
            Step::UpdateIdent { name, op, prefix } => {
                let old = self.pop();
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let op_imm = self.builder.ins().iconst(types::I64, *op as i64);
                let prefix_imm = self.builder.ins().iconst(types::I64, *prefix as i64);
                let res = self.call_slow(
                    self.sig_call,
                    Helper::UpdateIdent,
                    &[name_imm, op_imm, prefix_imm, old],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::Return => {
                let value = self.pop();
                if self.has_try || self.has_for_of {
                    // A return inside a try must run any finally first
                    // (and a finally's pending-return overrides an outer
                    // control); a return in a for-of body must close the
                    // active iterator before the body completes (spec
                    // 14.7.5.6 step 7). Both route through
                    // `return_control`, which runs `control_transfer`.
                    let ip = self.builder.ins().iconst(types::I64, (index + 1) as i64);
                    self.emit_dispatch_call(self.sig_update, Helper::ReturnControl, &[ip, value])?;
                } else {
                    self.builder.ins().return_(&[value]);
                }
            }
            Step::Throw => {
                let value = self.pop();
                let ip = self.builder.ins().iconst(types::I64, (index + 1) as i64);
                self.emit_dispatch_call(self.sig_update, Helper::ThrowControl, &[ip, value])?;
            }
            Step::Break { target } => {
                let ip = self.builder.ins().iconst(types::I64, (index + 1) as i64);
                let target_imm = self.builder.ins().iconst(types::I64, *target as i64);
                self.emit_dispatch_call(self.sig_update, Helper::BreakControl, &[ip, target_imm])?;
            }
            Step::Continue { target } => {
                let ip = self.builder.ins().iconst(types::I64, (index + 1) as i64);
                let target_imm = self.builder.ins().iconst(types::I64, *target as i64);
                self.emit_dispatch_call(
                    self.sig_update,
                    Helper::ContinueControl,
                    &[ip, target_imm],
                )?;
            }
            Step::EnterTry { handler } => {
                // Cut 70: snapshot the working-sp at the try entry — a
                // covered error resumes the catch/finally at this depth
                // (see the reset at the handler-entry steps).
                let sp = self.builder.use_var(self.sp_var);
                self.builder.def_var(self.handler_sp_vars[*handler], sp);
                let handler_imm = self.builder.ins().iconst(types::I64, *handler as i64);
                let _res = self.call_slow(self.sig_bool, Helper::EnterTry, &[handler_imm])?;
                self.fall_through(index);
            }
            Step::Exit { after } => {
                let ip = self.builder.ins().iconst(types::I64, (index + 1) as i64);
                let after_imm = self.builder.ins().iconst(types::I64, *after as i64);
                self.emit_dispatch_call(self.sig_update, Helper::ExitTry, &[ip, after_imm])?;
            }
            Step::FinallyEnd => {
                let ip = self.builder.ins().iconst(types::I64, (index + 1) as i64);
                self.emit_dispatch_call(self.sig_bool, Helper::FinallyEnd, &[ip])?;
            }
            // Suspension (Cut 58): `Yield`/`Await` pop the value, record
            // the suspension (value + working-sp + continuation step) via
            // the helper, and return `DISPATCH_SUSPEND` — the machine code
            // ends the segment; the driver saves the Vm (with the working
            // region) and `run_jit_resume` re-enters at the continuation.
            Step::Yield { delegate } => {
                let value = self.pop();
                let sp = self.builder.use_var(self.sp_var);
                let ip = self.builder.ins().iconst(types::I64, (index + 1) as i64);
                let delegate_imm = self.builder.ins().iconst(types::I64, i64::from(*delegate));
                let res = self.emit_raw_call(
                    self.sig_call,
                    Helper::YieldSuspend,
                    &[sp, value, delegate_imm, ip],
                )?;
                self.builder.ins().return_(&[res]);
            }
            Step::Await => {
                let value = self.pop();
                let sp = self.builder.use_var(self.sp_var);
                let ip = self.builder.ins().iconst(types::I64, (index + 1) as i64);
                let res =
                    self.emit_raw_call(self.sig_rel, Helper::AwaitSuspend, &[sp, value, ip])?;
                self.builder.ins().return_(&[res]);
            }
            // Destructuring (Cut 59): the primitive `Destructure*` steps
            // drive the pattern's iterator / object machinery through helpers
            // that mirror the interpreter handlers exactly (the shared Vm
            // stacks they manage are the authoritative state). The element /
            // key / rest values ride the working stack.
            Step::DestructureBegin => {
                let value = self.pop();
                let _res = self.call_slow(self.sig_bool, Helper::DestructureBegin, &[value])?;
                self.fall_through(index);
            }
            Step::DestructureNext => {
                let res = self.call_slow(self.sig_tdz, Helper::DestructureNext, &[])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::DestructureUndef { use_default } => {
                // Pop the value; if it is undefined, consume it and jump to
                // the default initializer (its own block — a static target);
                // otherwise leave it on the stack and fall through. The
                // consume happens in a dedicated block so the fall-through's
                // sp is untouched.
                let value = self.top();
                let is_undef = self
                    .builder
                    .ins()
                    .icmp_imm_u(IntCC::Equal, value, self.undef_bits);
                let default_block = self.ensure_block(*use_default);
                let next_block = self.ensure_block(index + 1);
                let pop_block = self.builder.create_block();
                self.builder
                    .ins()
                    .brif(is_undef, pop_block, &[], next_block, &[]);
                self.builder.switch_to_block(pop_block);
                self.pop();
                self.builder.ins().jump(default_block, &[]);
                self.builder.seal_block(pop_block);
            }
            Step::DestructureRest => {
                let res = self.call_slow(self.sig_tdz, Helper::DestructureRest, &[])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::DestructureObjCoercible => {
                let value = self.pop();
                let _res =
                    self.call_slow(self.sig_bool, Helper::DestructureObjCoercible, &[value])?;
                self.fall_through(index);
            }
            Step::DestructureObjKey { .. } => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let res = self.call_slow(self.sig_step, Helper::DestructureObjKey, &[step_imm])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::DestructureObjKeyComputed => {
                let key = self.pop();
                let res =
                    self.call_slow(self.sig_bool, Helper::DestructureObjKeyComputed, &[key])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::DestructureObjKeyStore => {
                let key = self.pop();
                let _res = self.call_slow(self.sig_bool, Helper::DestructureObjKeyStore, &[key])?;
                self.fall_through(index);
            }
            Step::DestructureObjKeyGet => {
                let res = self.call_slow(self.sig_tdz, Helper::DestructureObjKeyGet, &[])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::DestructureObjRest { .. } => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let res = self.call_slow(self.sig_step, Helper::DestructureObjRest, &[step_imm])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::DestructureClose => {
                let _res = self.call_slow(self.sig_tdz, Helper::DestructureClose, &[])?;
                self.fall_through(index);
            }
            Step::DestructureObjEnd => {
                let _res = self.call_slow(self.sig_tdz, Helper::DestructureObjEnd, &[])?;
                self.fall_through(index);
            }
            // Arguments object (Cut 60): `CreateArguments` builds the
            // body's `arguments` object (sloppy mapped — aliasing the
            // formals through the capture context — or strict unmapped) and
            // stores it into the frame slot; a step-index helper reads the
            // `slot`/`mapped` payload. Emitted once at body entry.
            Step::CreateArguments { .. } => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let _res = self.call_slow(self.sig_step, Helper::CreateArguments, &[step_imm])?;
                self.fall_through(index);
            }
            // `typeof` of a value operand (Cut 60): a pure helper — pops the
            // value, pushes the `typeof` string. The unresolvable-reference
            // form (`TypeofIdent`) stays env-path.
            Step::TypeofTop => {
                let value = self.pop();
                let res = self.emit_raw_call(self.sig_bool, Helper::TypeofTop, &[value])?;
                self.push(res);
                self.fall_through(index);
            }
            // Super property access (Cut 61): the helpers mirror the
            // interpreter handlers, branching the this binding / base between
            // the certified frame slot + home-object prototype and the
            // env-path Function env. The base rides the working stack; the
            // computed Keep leaves the CONVERTED key for the write (the
            // helper writes it at the passed sp; the machine code advances sp
            // past it, then pushes the read value).
            Step::GetSuperBase => {
                let res = self.call_slow(self.sig_tdz, Helper::GetSuperBase, &[])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::ThisValue => {
                let res = self.call_slow(self.sig_tdz, Helper::ThisValue, &[])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::GetSuperName { name } => {
                let base = self.pop();
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let res =
                    self.call_slow(self.sig_get_name, Helper::GetSuperName, &[base, name_imm])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::GetSuperComputed => {
                let key = self.pop();
                let base = self.pop();
                let res =
                    self.call_slow(self.sig_get_name, Helper::GetSuperComputed, &[base, key])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::GetSuperComputedKeep => {
                // `[base, key, base, key]` → `[base, key', value]`: pop the
                // top key + base + the write-copy key; the helper converts
                // the key once (the base stays the GetSuperBase capture) and
                // writes the converted key at the current sp; the machine
                // advances sp past it and pushes the read value.
                let key = self.pop();
                let base = self.pop();
                let _write_copy = self.pop();
                let sp = self.builder.use_var(self.sp_var);
                let res = self.call_slow(
                    self.sig_set_name,
                    Helper::GetSuperComputedKeep,
                    &[sp, base, key],
                )?;
                let sp2 = self.builder.ins().iadd_imm_s(sp, 8);
                self.builder.def_var(self.sp_var, sp2);
                self.push(res);
                self.fall_through(index);
            }
            Step::AssignSuperName { name, op } => {
                // `[base, old?, value]` → `[result]`.
                let value = self.pop();
                let old = if is_compound_assign(op) {
                    self.pop()
                } else {
                    self.builder.ins().iconst(types::I64, self.undef_bits)
                };
                let base = self.pop();
                let op_imm = self.builder.ins().iconst(types::I64, *op as u8 as i64);
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let res = self.call_slow(
                    self.sig_assign,
                    Helper::AssignSuperName,
                    &[op_imm, base, name_imm, old, value],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::AssignSuperComputed { op } => {
                // `[base, key, old?, value]` → `[result]`.
                let value = self.pop();
                let old = if is_compound_assign(op) {
                    self.pop()
                } else {
                    self.builder.ins().iconst(types::I64, self.undef_bits)
                };
                let key = self.pop();
                let base = self.pop();
                let op_imm = self.builder.ins().iconst(types::I64, *op as u8 as i64);
                let res = self.call_slow(
                    self.sig_assign,
                    Helper::AssignSuperComputed,
                    &[op_imm, base, key, old, value],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::UpdateSuperName { name, op, prefix } => {
                // `[base, old]` → `[result]`.
                let old = self.pop();
                let base = self.pop();
                let op_imm = self.builder.ins().iconst(types::I64, *op as u8 as i64);
                let prefix_imm = self.builder.ins().iconst(types::I64, i64::from(*prefix));
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let res = self.call_slow(
                    self.sig_assign,
                    Helper::UpdateSuperName,
                    &[op_imm, prefix_imm, base, name_imm, old],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::UpdateSuperComputed { op, prefix } => {
                // `[base, key, old]` → `[result]`.
                let old = self.pop();
                let key = self.pop();
                let base = self.pop();
                let op_imm = self.builder.ins().iconst(types::I64, *op as u8 as i64);
                let prefix_imm = self.builder.ins().iconst(types::I64, i64::from(*prefix));
                let res = self.call_slow(
                    self.sig_assign,
                    Helper::UpdateSuperComputed,
                    &[op_imm, prefix_imm, base, key, old],
                )?;
                self.push(res);
                self.fall_through(index);
            }
            Step::DeleteSuper => {
                // `delete super.x` always errors — the pending path surfaces
                // the ReferenceError.
                let _res = self.call_slow(self.sig_tdz, Helper::DeleteSuper, &[])?;
                self.fall_through(index);
            }
            Step::ResolveSuperRefName { name } => {
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let _res =
                    self.call_slow(self.sig_bool, Helper::ResolveSuperRefName, &[name_imm])?;
                self.fall_through(index);
            }
            Step::ResolveSuperRefComputed => {
                let key = self.pop();
                let base = self.pop();
                let _res = self.call_slow(
                    self.sig_get_name,
                    Helper::ResolveSuperRefComputed,
                    &[base, key],
                )?;
                self.fall_through(index);
            }
            Step::CatchBind { .. } => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let _res = self.call_slow(self.sig_bool, Helper::CatchBind, &[step_imm])?;
                self.fall_through(index);
            }
            Step::EnterBlock { .. } => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let _res = self.call_slow(self.sig_bool, Helper::EnterBlock, &[step_imm])?;
                self.fall_through(index);
            }
            Step::LeaveBlock => {
                let _res = self.call_slow(self.sig_tdz, Helper::LeaveBlock, &[])?;
                self.fall_through(index);
            }
            Step::SaveCompletion | Step::RestoreCompletion => {
                // The completion register is unobservable in the certified
                // model (the pending control carries the finally's deferred
                // return/throw; the body result comes from the machine-code
                // return), so the finally's save/restore round trip is a
                // no-op — same treatment as `ResetCompletion`.
                self.fall_through(index);
            }
            Step::SwitchDisc => {
                let disc = self.pop();
                let _res = self.call_slow(self.sig_bool, Helper::SwitchDisc, &[disc])?;
                self.fall_through(index);
            }
            Step::SwitchTest { case } => {
                // Strictly-equal the case test against the stored
                // discriminant; a match jumps to the case block (a static
                // target), otherwise the next test (or the default jump)
                // runs.
                let test = self.pop();
                let case_imm = self.builder.ins().iconst(types::I64, *case as i64);
                let matched =
                    self.call_slow(self.sig_update, Helper::SwitchTest, &[case_imm, test])?;
                let target = self.ensure_block(*case);
                self.cond_jump(matched, true, target, index + 1);
            }
            // for-in/for-of machinery (Cut 57): the begin steps open the
            // enumeration/iteration (the RHS was pushed by the head
            // expression); the fetch steps advance it — the helper writes
            // the element to the working stack (or the fused slot) and
            // returns 1, or returns 0 on exhaustion, and the machine code
            // jumps to `back`/`done` with static conditional branches (the
            // Cut 56 switch pattern); the binds land the element; the close
            // pops the boundary + iterator. `EnterPerIteration`/
            // `PerIteration` create the certified loop's per-iteration envs
            // (step-index helpers, the Cut 44 pattern).
            Step::ForInBegin => {
                let value = self.pop();
                let _res = self.call_slow(self.sig_bool, Helper::ForInBegin, &[value])?;
                self.fall_through(index);
            }
            Step::ForInNext { done, back } => {
                self.emit_for_fetch(*back, *done, Helper::ForInNext)?;
            }
            Step::ForOfBegin { .. } => {
                let value = self.pop();
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let _res =
                    self.call_slow(self.sig_get_name, Helper::ForOfBegin, &[step_imm, value])?;
                self.fall_through(index);
            }
            Step::ForOfNext { done, back } => {
                self.emit_for_fetch(*back, *done, Helper::ForOfNext)?;
            }
            Step::ForOfNextBindLocal { slot, done, back } => {
                let slot_imm = self.builder.ins().iconst(types::I64, *slot as i64);
                let code =
                    self.call_slow(self.sig_bool, Helper::ForOfNextBindLocal, &[slot_imm])?;
                let back_block = self.ensure_block(*back);
                let done_block = self.ensure_block(*done);
                let is_elem = self.builder.ins().icmp_imm_u(IntCC::Equal, code, 1);
                self.builder
                    .ins()
                    .brif(is_elem, back_block, &[], done_block, &[]);
            }
            Step::ForOfBindLocal { slot } => {
                // The element writes the frame slot directly (the write IS
                // the initialization — no TDZ check), mirroring the
                // interpreter handler.
                let value = self.pop();
                self.store_slot(*slot, value);
                self.fall_through(index);
            }
            Step::ForOfBindGlobal { name } => {
                // `store_global_value` — the `set_global` helper's exact
                // semantics (a declared global write through the cell
                // machinery).
                let value = self.pop();
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let _res =
                    self.call_slow(self.sig_get_name, Helper::SetGlobal, &[name_imm, value])?;
                self.fall_through(index);
            }
            Step::ForOfClose => {
                let _res = self.call_slow(self.sig_tdz, Helper::ForOfClose, &[])?;
                self.fall_through(index);
            }
            Step::EnterPerIteration { .. } => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let _res = self.call_slow(self.sig_step, Helper::EnterPerIteration, &[step_imm])?;
                self.fall_through(index);
            }
            Step::PerIteration { .. } => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let _res = self.call_slow(self.sig_step, Helper::PerIteration, &[step_imm])?;
                self.fall_through(index);
            }
            // Closure creation (Cut 44): the helper reads the step's payload
            // (the function AST, strictness, and the enclosing-chain layouts)
            // back out of the running body by index and instantiates the
            // closure against the CURRENT lexical environment — exactly the
            // interpreter's `Step::CreateFunction`/`CreateArrow` arms. The
            // created closure's own body compiles separately (the per-site
            // shared compiled body), so a loop creating closures now runs
            // entirely in machine code.
            Step::CreateFunction { .. } => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let res = self.call_slow(self.sig_step, Helper::CreateFunction, &[step_imm])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::CreateArrow { .. } => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let res = self.call_slow(self.sig_step, Helper::CreateArrow, &[step_imm])?;
                self.push(res);
                self.fall_through(index);
            }
            // The hoisted declaration stores into its frame or context slot
            // inside the helper (mirrors the interpreter arm); the step
            // completes with no value.
            Step::FunctionDeclInit { .. } => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let _res =
                    self.call_slow(self.sig_step, Helper::CreateFunctionDecl, &[step_imm])?;
                self.fall_through(index);
            }
            Step::NewTarget => {
                let res = self.call_slow(self.sig_tdz, Helper::NewTarget, &[])?;
                self.push(res);
                self.fall_through(index);
            }
            Step::RegExpLiteral { .. } => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let res = self.call_slow(self.sig_step, Helper::RegExpLiteral, &[step_imm])?;
                self.push(res);
                self.fall_through(index);
            }
            // Array literals (Cut 52): one helper per step, mirroring the
            // interpreter's handlers — `ArrayBegin` creates the array and
            // opens an index on the Vm's array-index stack; the element/
            // spread helpers define elements at that index; `ArrayEnd` pops
            // it and sets `length`. The array rides the work stack between
            // the steps (a heap object, so it cannot be a register value).
            Step::ArrayBegin => {
                let array = self.call_slow(self.sig_tdz, Helper::ArrayBegin, &[])?;
                self.push(array);
                self.fall_through(index);
            }
            Step::ArrayElement => {
                let value = self.pop();
                let array = self.pop();
                let array =
                    self.call_slow(self.sig_get_comp, Helper::ArrayElement, &[array, value])?;
                self.push(array);
                self.fall_through(index);
            }
            Step::ArrayHole => {
                // Only the index moves — the array stays on the work stack.
                self.call_slow(self.sig_tdz, Helper::ArrayHole, &[])?;
                self.fall_through(index);
            }
            Step::ArraySpread => {
                let iterable = self.pop();
                let array = self.pop();
                let array =
                    self.call_slow(self.sig_get_comp, Helper::ArraySpread, &[array, iterable])?;
                self.push(array);
                self.fall_through(index);
            }
            Step::ArrayEnd => {
                let array = self.pop();
                let array = self.call_slow(self.sig_bool, Helper::ArrayEnd, &[array])?;
                self.push(array);
                self.fall_through(index);
            }
            // Object literals (Cut 53): one helper per step, mirroring the
            // interpreter's handlers — `ObjectBegin` creates the plain
            // object; the init/method/accessor steps define the properties
            // with the object riding the work stack; `ObjectSpread` copies a
            // source's own enumerable properties. The method/accessor steps
            // pass their step index (the function/param/body payload is read
            // back from the running body, the Cut 44 pattern).
            Step::ObjectBegin => {
                let object = self.call_slow(self.sig_tdz, Helper::ObjectBegin, &[])?;
                self.push(object);
                self.fall_through(index);
            }
            Step::ObjectInitName {
                name,
                set_name,
                shorthand,
            } => {
                let value = self.pop();
                let object = self.pop();
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let set_imm = self.builder.ins().iconst(types::I64, i64::from(*set_name));
                let short_imm = self.builder.ins().iconst(types::I64, i64::from(*shorthand));
                let object = self.call_slow(
                    self.sig_assign,
                    Helper::ObjectInitName,
                    &[object, name_imm, set_imm, short_imm, value],
                )?;
                self.push(object);
                self.fall_through(index);
            }
            Step::ObjectInitComputed { set_name } => {
                let value = self.pop();
                let key = self.pop();
                let object = self.pop();
                let set_imm = self.builder.ins().iconst(types::I64, i64::from(*set_name));
                let object = self.call_slow(
                    self.sig_call,
                    Helper::ObjectInitComputed,
                    &[object, key, set_imm, value],
                )?;
                self.push(object);
                self.fall_through(index);
            }
            Step::ObjectKeyToPropertyKey => {
                let key = self.pop();
                let key = self.call_slow(self.sig_bool, Helper::ObjectKeyToPropertyKey, &[key])?;
                self.push(key);
                self.fall_through(index);
            }
            Step::ObjectMethodName { .. } => {
                let object = self.pop();
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let object = self.call_slow(
                    self.sig_get_name,
                    Helper::ObjectMethodName,
                    &[object, step_imm],
                )?;
                self.push(object);
                self.fall_through(index);
            }
            Step::ObjectMethodComputed { .. } => {
                let key = self.pop();
                let object = self.pop();
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let object = self.call_slow(
                    self.sig_set_name,
                    Helper::ObjectMethodComputed,
                    &[object, key, step_imm],
                )?;
                self.push(object);
                self.fall_through(index);
            }
            Step::ObjectAccessorName { .. } => {
                let object = self.pop();
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let object = self.call_slow(
                    self.sig_get_name,
                    Helper::ObjectAccessorName,
                    &[object, step_imm],
                )?;
                self.push(object);
                self.fall_through(index);
            }
            Step::ObjectAccessorComputed { .. } => {
                let key = self.pop();
                let object = self.pop();
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let object = self.call_slow(
                    self.sig_set_name,
                    Helper::ObjectAccessorComputed,
                    &[object, key, step_imm],
                )?;
                self.push(object);
                self.fall_through(index);
            }
            Step::ObjectSpread => {
                let from = self.pop();
                let object = self.pop();
                let object =
                    self.call_slow(self.sig_get_comp, Helper::ObjectSpread, &[object, from])?;
                self.push(object);
                self.fall_through(index);
            }
            // String literals (Cut 54): `PushStr` reads the literal's
            // `JsString` back from the running body and wraps it in a value;
            // `ConcatStr`/`ConcatStrConst` run the interpreter's flatten
            // template concat (the accumulator below the value).
            Step::PushStr(_) => {
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let value = self.call_slow(self.sig_step, Helper::PushStr, &[step_imm])?;
                self.push(value);
                self.fall_through(index);
            }
            Step::ConcatStr => {
                let value = self.pop();
                let acc = self.pop();
                let text = self.call_slow(self.sig_get_comp, Helper::ConcatStr, &[value, acc])?;
                self.push(text);
                self.fall_through(index);
            }
            Step::ConcatStrConst(_) => {
                let acc = self.pop();
                let step_imm = self.builder.ins().iconst(types::I64, index as i64);
                let text =
                    self.call_slow(self.sig_get_name, Helper::ConcatStrConst, &[acc, step_imm])?;
                self.push(text);
                self.fall_through(index);
            }
            // Completion bookkeeping: `ResetCompletion`/`NormalizeCompletion`
            // (and the list scopes) only touch the completion register, but
            // `SetCompletion` POPS the statement's value — a no-op would
            // leave the slot and drift the JIT stack one entry per
            // expression statement (catastrophic in a loop). Cut 65: a
            // certified SCRIPT completes through the register at fall-off-end,
            // so the write steps mirror the interpreter and store the real
            // value/empty flag into `vm.completion`/`completion_is_empty`
            // (functions never observe the register — their result comes from
            // the machine-code return — so the stores are harmless there).
            // `NormalizeCompletion` and the list scopes stay no-ops: in a
            // certified body the register is unobservable mid-run, and at
            // fall-off-end `Empty` and `Normal(undefined)` (what a normalize
            // of an empty register produces) convert to the same top-level
            // value, so skipping the normalize and the list save/restore
            // cannot change the completed value.
            Step::SetCompletion => {
                let value = self.pop();
                self.emit_completion_store(value, false);
                self.fall_through(index);
            }
            Step::ResetCompletion => {
                let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
                self.emit_completion_store(undef, true);
                self.fall_through(index);
            }
            Step::NormalizeCompletion | Step::ListBegin | Step::ListEnd => {
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

    /// The for-in/for-of protocol fetch (Cut 57): call the advance helper
    /// with the current working-stack pointer — on an element (code 1) it
    /// wrote the value at `sp[0]`, so advance the stack and jump to `back`
    /// (the head bind / body start); on exhaustion (code 0) jump to `done`.
    /// `done` is always a forward label (the certified loop places it after
    /// the loop), so its block is sealed at its own visit; `back` may be a
    /// back edge (the loop-bottom fetch's do-while target), already in
    /// `back_targets` via `step_targets`.
    fn emit_for_fetch(
        &mut self,
        back: usize,
        done: usize,
        helper: Helper,
    ) -> Result<(), Unsupported> {
        let sp = self.builder.use_var(self.sp_var);
        let code = self.call_slow(self.sig_bool, helper, &[sp])?;
        let back_block = self.ensure_block(back);
        let done_block = self.ensure_block(done);
        let elem_block = self.builder.create_block();
        let is_elem = self.builder.ins().icmp_imm_u(IntCC::Equal, code, 1);
        self.builder
            .ins()
            .brif(is_elem, elem_block, &[], done_block, &[]);
        self.builder.switch_to_block(elem_block);
        let sp = self.builder.use_var(self.sp_var);
        let next = self.builder.ins().iadd_imm_s(sp, 8);
        self.builder.def_var(self.sp_var, next);
        self.builder.ins().jump(back_block, &[]);
        self.builder.seal_block(elem_block);
        Ok(())
    }

    /// Cut 58: the entry's abrupt-resume path — restore the working sp,
    /// then route the resume value through the throw/return control
    /// machinery (mirroring `vm.run_abrupt` for a resumed plain
    /// `yield`/`await`). The machinery returns a step index or a completion
    /// sentinel; the dispatch branches over the body's static transfer
    /// targets.
    fn emit_resume_abrupt(&mut self, helper: Helper) -> Result<(), Unsupported> {
        let vm = self.vm();
        let sp = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            vm,
            Offset32::new(std::mem::offset_of!(JitCallContext, resume_sp) as i32),
        );
        self.builder.def_var(self.sp_var, sp);
        let ip = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            vm,
            Offset32::new(std::mem::offset_of!(JitCallContext, resume_ip) as i32),
        );
        let value = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            vm,
            Offset32::new(std::mem::offset_of!(JitCallContext, resume_value) as i32),
        );
        let res = self.emit_raw_call(self.sig_update, helper, &[ip, value])?;
        self.bump_leaf_epoch();
        self.emit_dispatch(res);
        Ok(())
    }

    /// Cut 58: the entry's normal-resume compare chain — jump to the block
    /// for the suspension continuation `resume_ip` matches (a target outside
    /// the body's static suspension set returns `undefined` defensively —
    /// the driver only resumes at a continuation).
    fn emit_resume_chain(&mut self, resume_ip: ClifValue) {
        let targets: Vec<usize> = self.suspension_targets.clone();
        if targets.is_empty() {
            let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
            self.builder.ins().return_(&[undef]);
            return;
        }
        let default = self.builder.create_block();
        let mut chain = self.builder.create_block();
        self.builder.ins().jump(chain, &[]);
        for (i, t) in targets.iter().enumerate() {
            self.builder.switch_to_block(chain);
            let eq = self
                .builder
                .ins()
                .icmp_imm_u(IntCC::Equal, resume_ip, *t as i64);
            let block = self.ensure_block(*t);
            if i + 1 == targets.len() {
                self.builder.ins().brif(eq, block, &[], default, &[]);
            } else {
                let next = self.builder.create_block();
                self.builder.ins().brif(eq, block, &[], next, &[]);
                chain = next;
            }
        }
        self.builder.switch_to_block(default);
        let undef = self.builder.ins().iconst(types::I64, self.undef_bits);
        self.builder.ins().return_(&[undef]);
        self.builder.seal_block(default);
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
    /// to `body_start` (pass) or `after` (fail). The limit is a literal, a
    /// frame slot, or a declared global — the binding forms re-read it every
    /// iteration, matching the general path's per-iteration evaluation (a
    /// body can mutate the limit).
    fn emit_fast_loop_head(
        &mut self,
        var: FastLoopVar,
        op: BinaryOp,
        limit: RelLimit,
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
                match limit {
                    RelLimit::Imm(imm) => {
                        let cc = rel_cc(op)?;
                        let imm_num = self.builder.ins().f64const(imm);
                        let cmp = self.builder.ins().fcmp(cc, cur, imm_num);
                        let test = self.bint(cmp);
                        self.cond_jump(test, true, body_block, after);
                        Ok(())
                    }
                    _ => {
                        let limit_bits = self.emit_rel_limit_bits(limit)?;
                        let counter_bits = self.counter_bits();
                        let test = self.emit_rel_test2(op, counter_bits, limit_bits)?;
                        self.cond_jump(test, true, body_block, after);
                        Ok(())
                    }
                }
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
                // The limit, read once per iteration (a body can mutate it):
                // a constant for the literal shape, else the binding value
                // and its number-ness for the fast-path gate.
                let limit_bits = self.emit_rel_limit_bits(limit)?;
                let limit_is_num = match limit {
                    RelLimit::Imm(_) => self.builder.ins().iconst(types::I8, 1),
                    _ => self.is_double(limit_bits),
                };
                let both = self.builder.ins().band(is_num, limit_is_num);
                let cc = rel_cc(op)?;
                let limit_num =
                    self.builder
                        .ins()
                        .bitcast(types::F64, MemFlagsData::new(), limit_bits);
                let cmp = self.builder.ins().fcmp(cc, new_num, limit_num);
                let test_fast = self.bint(cmp);
                let new_var = self.builder.declare_var(types::I64);
                let test_var = self.builder.declare_var(types::I64);
                self.builder.def_var(new_var, new_fast);
                self.builder.def_var(test_var, test_fast);
                let merge = self.builder.create_block();
                let slow = self.builder.create_block();
                self.builder.ins().brif(both, merge, &[], slow, &[]);
                self.builder.switch_to_block(slow);
                let inc_imm = self.builder.ins().iconst(types::I64, inc as i64);
                let new_slow =
                    self.call_slow(self.sig_update, Helper::UpdateValueSlow, &[inc_imm, old])?;
                let op_imm = self.builder.ins().iconst(types::I64, op as i64);
                let test_slow = self.call_slow(
                    self.sig_rel,
                    Helper::RelationalSlow,
                    &[op_imm, new_slow, limit_bits],
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

    /// The limit operand of a fused relational loop head/test: the value
    /// bits of a frame slot or declared global (a literal is handled by the
    /// `JumpIf*Imm`/imm head paths — this is only called for the binding
    /// forms, except the Slot head's uniform path which also passes the
    /// literal).
    fn emit_rel_limit_bits(&mut self, limit: RelLimit) -> Result<ClifValue, Unsupported> {
        match limit {
            RelLimit::Imm(imm) => self.const_value(&Value::Number(imm)),
            RelLimit::Slot(slot) => {
                let bits = self.load_slot(slot);
                if self.slot_is_lexical(slot) {
                    self.emit_tdz_check(bits)?;
                }
                Ok(bits)
            }
            RelLimit::Global(name) => self.emit_global_read(name),
        }
    }

    /// The JS relational test `left <op> right` as a 0/1 I64: the
    /// both-number fast path inline, otherwise `relational_slow` (the
    /// two-value sibling of `emit_rel_test`, which compares against a
    /// constant).
    fn emit_rel_test2(
        &mut self,
        op: BinaryOp,
        left: ClifValue,
        right: ClifValue,
    ) -> Result<ClifValue, Unsupported> {
        let cc = rel_cc(op)?;
        let left_num = self.is_double(left);
        let right_num = self.is_double(right);
        let both = self.builder.ins().band(left_num, right_num);
        let left_f = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), left);
        let right_f = self
            .builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), right);
        let cmp = self.builder.ins().fcmp(cc, left_f, right_f);
        let test_fast = self.bint(cmp);
        let test_var = self.builder.declare_var(types::I64);
        self.builder.def_var(test_var, test_fast);
        let merge = self.builder.create_block();
        let slow = self.builder.create_block();
        self.builder.ins().brif(both, merge, &[], slow, &[]);
        self.builder.switch_to_block(slow);
        let op_imm = self.builder.ins().iconst(types::I64, op as i64);
        let test_slow =
            self.call_slow(self.sig_rel, Helper::RelationalSlow, &[op_imm, left, right])?;
        self.builder.def_var(test_var, test_slow);
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(merge);
        self.builder.switch_to_block(merge);
        Ok(self.builder.use_var(test_var))
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
        // The slow branch: for `Add`, the string-string rope concat runs
        // through a direct helper (Cut 41 — the compiled code checks both
        // operands' string tags, so `apply_binary`'s dispatch and number
        // checks are skipped); everything else goes to `binary_slow`.
        self.builder.switch_to_block(slow);
        if op == BinaryOp::Add {
            let lhs_str = self.is_string(lhs);
            let rhs_str = self.is_string(rhs);
            let both_str = self.builder.ins().band(lhs_str, rhs_str);
            let concat = self.builder.create_block();
            let str_slow = self.builder.create_block();
            self.builder
                .ins()
                .brif(both_str, concat, &[], str_slow, &[]);
            self.builder.switch_to_block(concat);
            let concat_res =
                self.call_slow(self.sig_get_name, Helper::ConcatStrings, &[lhs, rhs])?;
            self.builder.def_var(res_var, concat_res);
            self.builder.ins().jump(merge, &[]);
            self.builder.switch_to_block(str_slow);
        }
        let op_imm = self.builder.ins().iconst(types::I64, op as i64);
        let slow_res = self.call_slow(self.sig_binary, Helper::BinarySlow, &[op_imm, lhs, rhs])?;
        self.builder.def_var(res_var, slow_res);
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(merge);
        self.builder.switch_to_block(merge);
        Ok(self.builder.use_var(res_var))
    }

    // ----- register body (LeafOp) lowering -----

    fn emit_leaf_op(
        &mut self,
        step: usize,
        op_index: usize,
        op: &LeafOp,
    ) -> Result<(), Unsupported> {
        match op {
            LeafOp::LoadReg { slot, tdz } => {
                let bits = self.load_slot(*slot);
                if *tdz {
                    self.emit_tdz_check(bits)?;
                }
                self.builder.def_var(self.acc_var, bits);
            }
            LeafOp::LoadContext { index } => {
                // Depth-0 capture-context read through the slow path.
                let zero = self.builder.ins().iconst(types::I64, 0);
                let index_imm = self.builder.ins().iconst(types::I64, *index as i64);
                let res =
                    self.call_slow(self.sig_get_name, Helper::LoadContext, &[zero, index_imm])?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::LoadPerIter { index } => {
                // Depth-0 per-iteration read through the slow path (the
                // leaf's `lexical_env` is its captured per-iteration env).
                let zero = self.builder.ins().iconst(types::I64, 0);
                let index_imm = self.builder.ins().iconst(types::I64, *index as i64);
                let res =
                    self.call_slow(self.sig_get_name, Helper::LoadPerIter, &[zero, index_imm])?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::LoadCounter => {
                let bits = self.counter_bits();
                self.builder.def_var(self.acc_var, bits);
            }
            LeafOp::LoadConst(value) => {
                let bits = self.const_bits(value, step, op_index, 0)?;
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
            LeafOp::BinContext { op, index } => {
                // `acc = acc op context[index]` (the fused `LoadContext` +
                // binary — the register executor's evaluation order).
                let zero = self.builder.ins().iconst(types::I64, 0);
                let index_imm = self.builder.ins().iconst(types::I64, *index as i64);
                let right =
                    self.call_slow(self.sig_get_name, Helper::LoadContext, &[zero, index_imm])?;
                let left = self.builder.use_var(self.acc_var);
                let res = self.emit_binary(*op, left, right)?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::BinPerIter { op, index } => {
                // `acc = acc op per-iteration[index]` (depth 0).
                let zero = self.builder.ins().iconst(types::I64, 0);
                let index_imm = self.builder.ins().iconst(types::I64, *index as i64);
                let right =
                    self.call_slow(self.sig_get_name, Helper::LoadPerIter, &[zero, index_imm])?;
                let left = self.builder.use_var(self.acc_var);
                let res = self.emit_binary(*op, left, right)?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::BinCtxReg {
                op,
                index,
                slot,
                tdz,
            } => {
                // `acc = context[index] op frame[slot]` (captured left,
                // frame-slot right — the step path's evaluation order).
                let zero = self.builder.ins().iconst(types::I64, 0);
                let index_imm = self.builder.ins().iconst(types::I64, *index as i64);
                let left =
                    self.call_slow(self.sig_get_name, Helper::LoadContext, &[zero, index_imm])?;
                let right = self.load_slot(*slot);
                if *tdz {
                    self.emit_tdz_check(right)?;
                }
                let res = self.emit_binary(*op, left, right)?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::BinImm { op, imm } => {
                let left = self.builder.use_var(self.acc_var);
                let right = self.const_value(&Value::Number(*imm))?;
                let res = self.emit_binary(*op, left, right)?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::BinConst { op, value } => {
                let left = self.builder.use_var(self.acc_var);
                let right = self.const_bits(value, step, op_index, 0)?;
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
            LeafOp::UpdateAcc { op } => {
                // acc = ToNumeric(acc) ± 1 (`o.x++`'s update half): the
                // number case is the inline f64 add (mirrors `emit_update`),
                // anything else falls to the general `UpdateValueSlow`.
                let old = self.builder.use_var(self.acc_var);
                let is_num = self.is_double(old);
                let num = self
                    .builder
                    .ins()
                    .bitcast(types::F64, MemFlagsData::new(), old);
                let delta = if matches!(op, syntax::ast::UpdateOp::Increment) {
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
                let inc_imm = self.builder.ins().iconst(types::I64, *op as i64);
                let new_slow =
                    self.call_slow(self.sig_update, Helper::UpdateValueSlow, &[inc_imm, old])?;
                self.builder.def_var(new_var, new_slow);
                self.builder.ins().jump(merge, &[]);
                self.builder.seal_block(merge);
                self.builder.switch_to_block(merge);
                let new_value = self.builder.use_var(new_var);
                self.builder.def_var(self.acc_var, new_value);
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
                let value = self.leaf_operand(step, op_index, 1, value)?;
                let name = self.builder.ins().iconst(types::I64, *name as i64);
                self.call_slow(
                    self.sig_set_name,
                    Helper::SetMemberName,
                    &[object, name, value],
                )?;
            }
            LeafOp::StoreMemberNameLocal { object_slot, name } => {
                // The object is the frame slot (read at store time), the
                // value the accumulator. Inline the validated in-place
                // member store like the step path's member write: an
                // ordinary-object receiver whose member value cell
                // validates (an own writable data property at the current
                // generation — the run's preceding `GetMemberNameLocal`
                // read refreshed it) writes through `set_member_slot` (no
                // generation bump, no [[Set]] walk); every other receiver
                // falls to the full `SetMemberName` helper (nullish check
                // + assign machinery).
                let object = self.load_slot(*object_slot);
                let value = self.builder.use_var(self.acc_var);
                let name_imm = self.builder.ins().iconst(types::I64, *name as i64);
                let ctx = self.vm();
                // The object-kind gate mirrors the step's `obj_ok`: the
                // payload must point at an object before the cell probe
                // dereferences it.
                let is_heap = self.builder.ins().band_imm_u(object, crux::TAG_MASK as i64);
                let is_obj =
                    self.builder
                        .ins()
                        .icmp_imm_u(IntCC::Equal, is_heap, crux::TAG_PREFIX as i64);
                let tag = self.builder.ins().ushr_imm_u(object, 44);
                let tag = self.builder.ins().band_imm_u(tag, 0xF);
                let tag_obj =
                    self.builder
                        .ins()
                        .icmp_imm_u(IntCC::Equal, tag, crux::TAG_OBJECT as i64);
                let obj_ok = self.builder.ins().band(is_obj, tag_obj);
                let validate = self.builder.create_block();
                let slow = self.builder.create_block();
                let merge = self.builder.create_block();
                self.builder.ins().brif(obj_ok, validate, &[], slow, &[]);
                // The cell probe (mirrors `GetMemberName`/the step store):
                // the live id/generation must match the cell the run's read
                // warmed on an own data-property read.
                self.builder.switch_to_block(validate);
                let cells = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    ctx,
                    Offset32::new(std::mem::offset_of!(JitCallContext, member_value_cells) as i32),
                );
                let ptr = self
                    .builder
                    .ins()
                    .band_imm_u(object, crux::PAYLOAD_MASK as i64);
                let ptr = self.builder.ins().ishl_imm_u(ptr, 4);
                // The payload stores the `GcBox` base; the `JsObject` sits
                // after the box header (`mark` + `size`).
                let ptr = self
                    .builder
                    .ins()
                    .iadd_imm_s(ptr, crux::heap::GCBOX_DATA_OFFSET as i64);
                let live_id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    ptr,
                    Offset32::new(std::mem::offset_of!(crux::JsObject, id) as i32),
                );
                let live_gen = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    ptr,
                    Offset32::new(std::mem::offset_of!(crux::JsObject, generation) as i32),
                );
                let cell_slot = self.builder.ins().bxor(live_id, name_imm);
                let cell_slot = self
                    .builder
                    .ins()
                    .band_imm_u(cell_slot, (MEMBER_CELLS - 1) as i64);
                let index_bytes = self
                    .builder
                    .ins()
                    .imul_imm_s(cell_slot, std::mem::size_of::<MemberValueCell>() as i64);
                let cell = self.builder.ins().iadd(cells, index_bytes);
                let cell_id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(MemberValueCell, id) as i32),
                );
                let cell_name = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(MemberValueCell, name) as i32),
                );
                let cell_gen = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    cell,
                    Offset32::new(std::mem::offset_of!(MemberValueCell, generation) as i32),
                );
                let id_ok = self.builder.ins().icmp(IntCC::Equal, cell_id, live_id);
                let name_ok = self
                    .builder
                    .ins()
                    .icmp_imm_u(IntCC::Equal, cell_name, *name as i64);
                let gen_ok = self.builder.ins().icmp(IntCC::Equal, cell_gen, live_gen);
                let id_name_ok = self.builder.ins().band(id_ok, name_ok);
                let ok = self.builder.ins().band(id_name_ok, gen_ok);
                let fast = self.builder.create_block();
                self.builder.ins().brif(ok, fast, &[], slow, &[]);
                // The fast path: the helper writes the property vector in
                // place and refreshes the cell (no generation bump). The
                // store's result is discarded by the run truncate.
                self.builder.switch_to_block(fast);
                let _stored = self.call_slow(
                    self.sig_set_name,
                    Helper::SetMemberSlot,
                    &[object, name_imm, value],
                )?;
                self.builder.ins().jump(merge, &[]);
                // The slow path: the full member store helper (nullish
                // check + assign machinery).
                self.builder.switch_to_block(slow);
                let _stored = self.call_slow(
                    self.sig_set_name,
                    Helper::SetMemberName,
                    &[object, name_imm, value],
                )?;
                self.builder.ins().jump(merge, &[]);
                self.builder.seal_block(merge);
                self.builder.switch_to_block(merge);
            }
            LeafOp::StoreMemberComputed { key, value } => {
                let object = self.builder.use_var(self.acc_var);
                let key = self.leaf_operand(step, op_index, 3, key)?;
                let value = self.leaf_operand(step, op_index, 4, value)?;
                self.emit_computed_store(object, key, value)?;
            }
            LeafOp::StoreMemberComputedLocal { object_slot, key } => {
                // The object is the frame slot (read at store time — the
                // late-read contract), the key a `Reg`/`Const` operand, the
                // value the accumulator. Same gates as `StoreMemberComputed`.
                let object = self.load_slot(*object_slot);
                let key = self.leaf_operand(step, op_index, 5, key)?;
                let value = self.builder.use_var(self.acc_var);
                self.emit_computed_store(object, key, value)?;
            }
            LeafOp::CompoundMemberComputedLocal {
                object_slot,
                key,
                op,
                rhs,
            } => {
                // One slow call: the helper converts the key once, reads the
                // old value, applies the compound to the RHS, and stores
                // through the same key. The key and the RHS are pure
                // operands (loaded here — stable across the helper's read).
                let object = self.load_slot(*object_slot);
                let key = self.leaf_operand(step, op_index, 6, key)?;
                let value = self.leaf_operand(step, op_index, 5, rhs)?;
                let op = self.builder.ins().iconst(types::I64, *op as i64);
                let _res = self.call_slow(
                    self.sig_rmw,
                    Helper::RmwCompoundComputed,
                    &[object, key, value, op],
                )?;
            }
            LeafOp::UpdateMemberComputedLocal {
                object_slot,
                key,
                op,
            } => {
                // One slow call: the helper converts the key once, applies
                // the ToNumeric update, and stores through the same key.
                let object = self.load_slot(*object_slot);
                let key = self.leaf_operand(step, op_index, 5, key)?;
                let op = self.builder.ins().iconst(types::I64, *op as i64);
                let _res = self.call_slow(
                    self.sig_set_comp,
                    Helper::RmwUpdateComputed,
                    &[object, key, op],
                )?;
            }
            LeafOp::GetMemberName { name } => {
                let object = self.builder.use_var(self.acc_var);
                let value = self.emit_member_cell_read(object, *name)?;
                self.builder.def_var(self.acc_var, value);
            }
            LeafOp::GetMemberComputed { key } => {
                let object = self.builder.use_var(self.acc_var);
                let key = self.leaf_operand(step, op_index, 2, key)?;
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
                let value = self.emit_member_cell_read(object, *name)?;
                self.builder.def_var(self.acc_var, value);
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
                let key = self.leaf_operand(step, op_index, 2, key)?;
                let res =
                    self.call_slow(self.sig_get_comp, Helper::GetMemberComputed, &[object, key])?;
                self.builder.def_var(self.acc_var, res);
            }
            LeafOp::PushAcc => {
                // The register executor pushes the accumulator (Cut 35 slice
                // 10 spill) — NOT the loop counter (that is `Step::PushAcc`,
                // which lowers to `LoadCounter` instead).
                let bits = self.builder.use_var(self.acc_var);
                self.push(bits);
            }
            LeafOp::ReturnAcc => {
                let value = self.builder.use_var(self.acc_var);
                self.builder.ins().return_(&[value]);
            }
        }
        Ok(())
    }

    /// The shared computed-member store tail (the register `StoreMemberComputed`
    /// and `StoreMemberComputedLocal` ops): the inline dense-array append gate
    /// (shared with the step path, gap-close M1 C), then the machine
    /// typed-array store (gap-close M5c); the remaining path is the full
    /// `SetMemberComputed` slow call (the register executor discards the
    /// store's result, so no push/merge value). `object`/`key`/`value` are
    /// already materialized values.
    fn emit_computed_store(
        &mut self,
        object: ClifValue,
        key: ClifValue,
        value: ClifValue,
    ) -> Result<(), Unsupported> {
        let ta_gate = self.builder.create_block();
        let slow = self.builder.create_block();
        let merge = self.builder.create_block();
        self.emit_dense_array_append_inline(object, key, value, ta_gate, merge)?;
        self.builder.switch_to_block(ta_gate);
        self.emit_typed_array_store_inline(object, key, value, slow, merge)?;
        self.builder.switch_to_block(slow);
        self.call_slow(
            self.sig_set_comp,
            Helper::SetMemberComputed,
            &[object, key, value],
        )?;
        self.builder.ins().jump(merge, &[]);
        self.builder.seal_block(slow);
        self.builder.switch_to_block(merge);
        Ok(())
    }

    /// Load a `RegOperand` (a member op's key/value): a frame slot (with its
    /// `tdz` check), a constant, or the loop counter. `step`/`op_index`
    /// locate the register op in the running body and `field` the
    /// const-bearing field, for the heap-constant fallback (Cut 54).
    fn leaf_operand(
        &mut self,
        step: usize,
        op_index: usize,
        field: u64,
        operand: &RegOperand,
    ) -> Result<ClifValue, Unsupported> {
        match operand {
            RegOperand::Reg { slot, tdz } => {
                let bits = self.load_slot(*slot);
                if *tdz {
                    self.emit_tdz_check(bits)?;
                }
                Ok(bits)
            }
            RegOperand::Const(value) => self.const_bits(value, step, op_index, field),
            RegOperand::Counter => Ok(self.counter_bits()),
            RegOperand::PostInc { slot, tdz, op } => {
                // A post-increment key: read the slot, write the update back,
                // yield the old value — the same fast/slow shape as
                // `emit_update` (the f64 add inlines; a non-number falls to
                // `UpdateValueSlow`). The write-back lands before the store
                // machinery runs, matching the interpreter's load ordering.
                let old = self.load_slot(*slot);
                if *tdz {
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
                let inc_imm = self.builder.ins().iconst(types::I64, *op as i64);
                let new_slow =
                    self.call_slow(self.sig_update, Helper::UpdateValueSlow, &[inc_imm, old])?;
                self.builder.def_var(new_var, new_slow);
                self.builder.ins().jump(merge, &[]);
                self.builder.seal_block(merge);
                self.builder.switch_to_block(merge);
                let new_value = self.builder.use_var(new_var);
                self.store_slot(*slot, new_value);
                Ok(old)
            }
            RegOperand::Ctx { index } => {
                // A captured binding as a member key/value: the depth-0
                // context read through the slow path.
                let zero = self.builder.ins().iconst(types::I64, 0);
                let index_imm = self.builder.ins().iconst(types::I64, *index as i64);
                self.call_slow(self.sig_get_name, Helper::LoadContext, &[zero, index_imm])
            }
            RegOperand::PerIter { index } => {
                // A captured for-head binding as a member key/value.
                let zero = self.builder.ins().iconst(types::I64, 0);
                let index_imm = self.builder.ins().iconst(types::I64, *index as i64);
                self.call_slow(self.sig_get_name, Helper::LoadPerIter, &[zero, index_imm])
            }
            RegOperand::Acc | RegOperand::Spilled => Err(Unsupported::Leaf("member operand shape")),
        }
    }

    /// Inline a constant's bits when it is a non-heap value, else read it
    /// back from the running body's register op at `(step, op_index)` via the
    /// `load_const` helper (`field` selects the const-bearing field). The
    /// compiled body holds the strong ref, mirroring the register executor's
    /// `RegOperand::Const(value) => Ok(*value)`.
    fn const_bits(
        &mut self,
        value: &Value,
        step: usize,
        op_index: usize,
        field: u64,
    ) -> Result<ClifValue, Unsupported> {
        match self.const_value(value) {
            Ok(bits) => Ok(bits),
            Err(_) => {
                let step_imm = self.builder.ins().iconst(types::I64, step as i64);
                let op_imm = self.builder.ins().iconst(types::I64, op_index as i64);
                let field_imm = self.builder.ins().iconst(types::I64, field as i64);
                self.call_slow(
                    self.sig_set_name,
                    Helper::LoadConst,
                    &[step_imm, op_imm, field_imm],
                )
            }
        }
    }
}
