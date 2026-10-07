//! The IR -> Cranelift lowering (`.notes/optimizing-tier-impl.md` §4).
//!
//! Increment I1: an identity lowering of the lifted straight-line IR, reusing
//! the helper table and the `JitCallContext` ABI unchanged, so a body lowered
//! here is interchangeable with a per-step one at a call boundary. Gated by
//! `SLAG_OPT` (see [`JitEngine`](crate::JitEngine)); a body `opt::lift` refuses
//! — or that this lowerer then declines — keeps the per-step path.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::Offset32;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, InstBuilder, MemFlagsData, SigRef, UserFuncName, Value as ClifValue,
    types,
};
use cranelift_codegen::isa::{CallConv, TargetIsa};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use crux::Value as JsValue;
use runtime::jit::{JitCallContext, VM_COMPLETION_IS_EMPTY_OFFSET, VM_COMPLETION_OFFSET};
use syntax::ast::{BinaryOp, UnaryOp};

use crate::Compiled;
use crate::compiler::{Unsupported, assemble, helper_sig, jit_sig, platform_call_conv};
use crate::helpers::{Helper, JitHelpers};
use crate::opt::ir::{BlockId, Function as IrFunction, Imm, Inst, Op, Term, Type, ValueId};

/// How many bodies the optimizing tier has lowered (test introspection).
#[cfg(test)]
pub(crate) static OPT_COMPILED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Lower a lifted straight-line IR function to executable code, or `None` when
/// the IR uses a shape this increment does not handle (the caller keeps the
/// per-step path).
pub fn compile(
    isa: &dyn TargetIsa,
    ir: &IrFunction,
    helpers: &JitHelpers,
    max_stack: usize,
) -> Option<Compiled> {
    let conv = platform_call_conv(isa);
    let mut func =
        Function::with_name_signature(UserFuncName::testcase("jit_opt_body"), jit_sig(conv));
    let mut fctx = FunctionBuilderContext::new();
    if lower(ir, helpers, &mut func, &mut fctx, conv).is_err() {
        return None;
    }
    let compiled = assemble(isa, func, max_stack)?;
    #[cfg(test)]
    OPT_COMPILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(compiled)
}

/// The entry ABI a lowered op reads: the frame base, the `JitCallContext`
/// pointer, and the shared helper signatures.
struct Abi {
    frame: ClifValue,
    vm: ClifValue,
    sig_binary: SigRef,
    sig_unary: SigRef,
    sig_bool: SigRef,
    sig_tdz: SigRef,
}

fn lower(
    ir: &IrFunction,
    helpers: &JitHelpers,
    func: &mut Function,
    fctx: &mut FunctionBuilderContext,
    conv: CallConv,
) -> Result<(), Unsupported> {
    let mut builder = FunctionBuilder::new(func, fctx);
    // Cranelift forbids jumping to the entry block, so when anything targets IR
    // block 0 (a loop back to the body start) the function entry is a prologue
    // that binds the parameters and jumps to a separate block 0.
    let loops_to_entry = (0..ir.block_count() as BlockId).any(|b| match &ir.block(b).term {
        Some(Term::Jump { target, .. }) => *target == 0,
        Some(Term::Branch {
            then_block,
            else_block,
            ..
        }) => *then_block == 0 || *else_block == 0,
        _ => false,
    });
    let func_entry = builder.create_block();
    builder.append_block_params_for_function_params(func_entry);
    // One Cranelift block per IR block; every block takes its IR parameters as
    // Cranelift block parameters.
    let mut clif: Vec<Block> = Vec::with_capacity(ir.block_count());
    if loops_to_entry {
        clif.push(builder.create_block());
    } else {
        clif.push(func_entry);
    }
    for b in 1..ir.block_count() as BlockId {
        let block = builder.create_block();
        for _ in 0..ir.block(b).params.len() {
            builder.append_block_param(block, types::I64);
        }
        clif.push(block);
    }

    builder.switch_to_block(func_entry);
    let entry_params = builder.block_params(func_entry).to_vec();
    let abi = Abi {
        frame: entry_params[0],
        vm: entry_params[2],
        sig_binary: builder.import_signature(helper_sig(&[types::I64; 4], conv)),
        sig_unary: builder.import_signature(helper_sig(&[types::I64; 3], conv)),
        sig_bool: builder.import_signature(helper_sig(&[types::I64; 2], conv)),
        sig_tdz: builder.import_signature(helper_sig(&[types::I64; 1], conv)),
    };
    let undefined = JsValue::Undefined.bits() as i64;
    if loops_to_entry {
        builder.ins().jump(clif[0], &[]);
    }

    let mut values: Vec<Option<ClifValue>> = vec![None; ir.value_count() as usize];
    let types = ir.value_types();
    for b in 0..ir.block_count() as BlockId {
        let block = ir.block(b);
        builder.switch_to_block(clif[b as usize]);
        let params = builder.block_params(clif[b as usize]).to_vec();
        for (i, p) in block.params.iter().enumerate() {
            values[*p as usize] = Some(params[i]);
        }
        for inst in &block.insts {
            let result = lower_inst(&mut builder, inst, &abi, helpers, &values, types)?;
            if let Some(id) = inst.result {
                values[id as usize] = Some(result);
            }
        }
        match &block.term {
            Some(Term::Return(value)) => {
                let v = match value {
                    Some(id) => value_of(&values, *id)?,
                    None => builder.ins().iconst(types::I64, undefined),
                };
                builder.ins().return_(&[v]);
            }
            Some(Term::Jump { target, args }) => {
                let vals = resolve(args, &values)?;
                builder.ins().jump(clif[*target as usize], &vals);
            }
            Some(Term::Branch {
                cond,
                then_block,
                then_args,
                else_block,
                else_args,
            }) => {
                let c = value_of(&values, *cond)?;
                let truthy = call_helper(
                    &mut builder,
                    helpers,
                    &abi,
                    abi.sig_bool,
                    Helper::ToBooleanSlow,
                    &[c],
                )?;
                let test = builder.ins().icmp_imm_u(IntCC::NotEqual, truthy, 0);
                let then_vals = resolve(then_args, &values)?;
                let else_vals = resolve(else_args, &values)?;
                builder.ins().brif(
                    test,
                    clif[*then_block as usize],
                    &then_vals,
                    clif[*else_block as usize],
                    &else_vals,
                );
            }
            _ => return Err(Unsupported::Step("opt:term")),
        }
    }
    builder.seal_all_blocks();
    Ok(())
}

fn resolve(args: &[ValueId], values: &[Option<ClifValue>]) -> Result<Vec<BlockArg>, Unsupported> {
    args.iter()
        .map(|id| value_of(values, *id).map(Into::into))
        .collect()
}

fn lower_inst(
    builder: &mut FunctionBuilder,
    inst: &Inst,
    abi: &Abi,
    helpers: &JitHelpers,
    values: &[Option<ClifValue>],
    types: &[Type],
) -> Result<ClifValue, Unsupported> {
    let arg = |i: usize| value_of(values, inst.args[i]);
    Ok(match inst.op {
        Op::Const => {
            let bits = match &inst.imm {
                Imm::Float(f) => JsValue::Number(*f).bits() as i64,
                Imm::Int(i) => JsValue::Number(*i as f64).bits() as i64,
                Imm::Bool(b) => JsValue::Boolean(*b).bits() as i64,
                _ => return Err(Unsupported::Step("opt:const")),
            };
            builder.ins().iconst(types::I64, bits)
        }
        Op::FrameLoad => {
            let Imm::Slot(slot) = &inst.imm else {
                return Err(Unsupported::Step("opt:frame"));
            };
            builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                abi.frame,
                Offset32::new((*slot as i32) * 8),
            )
        }
        Op::FrameStore => {
            let Imm::Slot(slot) = &inst.imm else {
                return Err(Unsupported::Step("opt:frame"));
            };
            let value = arg(0)?;
            builder.ins().store(
                MemFlagsData::new(),
                value,
                abi.frame,
                Offset32::new((*slot as i32) * 8),
            );
            // An effect-only op has no result; the caller ignores the return.
            builder
                .ins()
                .iconst(types::I64, JsValue::Undefined.bits() as i64)
        }
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Mod
        | Op::Pow
        | Op::BitAnd
        | Op::BitOr
        | Op::BitXor
        | Op::Shl
        | Op::Shr
        | Op::UShr
        | Op::Eq
        | Op::StrictEq
        | Op::Lt
        | Op::Le
        | Op::Gt
        | Op::Ge => {
            let disc = binary_op(inst.op).ok_or(Unsupported::Step("opt:binary"))?;
            let (lhs, rhs) = (arg(0)?, arg(1)?);
            let numeric = matches!(
                inst.op,
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Lt | Op::Le | Op::Gt | Op::Ge
            );
            // With both operands proven Numbers (the narrowing pass), the
            // arithmetic/comparison is a bare f64 op with no tag check, no slow
            // block and no branch — the same shape the per-step path reaches
            // with a known-number operand.
            let known = numeric
                && inst.args.iter().all(|a| {
                    types
                        .get(*a as usize)
                        .is_some_and(|t| matches!(t, Type::Number | Type::Int))
                });
            if known {
                emit_bare_numeric(builder, inst.op, lhs, rhs)
            } else if numeric {
                call_numeric_binary(builder, helpers, abi, inst.op, disc as i64, lhs, rhs)?
            } else if matches!(
                inst.op,
                Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr
            ) {
                call_int_binary(builder, helpers, abi, inst.op, disc as i64, lhs, rhs)?
            } else {
                let op = builder.ins().iconst(types::I64, disc as i64);
                call_helper(
                    builder,
                    helpers,
                    abi,
                    abi.sig_binary,
                    Helper::BinarySlow,
                    &[op, lhs, rhs],
                )?
            }
        }
        Op::ToNumber | Op::Neg | Op::BitNot | Op::Not => {
            let disc = unary_op(inst.op).ok_or(Unsupported::Step("opt:unary"))?;
            let op = builder.ins().iconst(types::I64, disc as i64);
            let value = arg(0)?;
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_unary,
                Helper::UnarySlow,
                &[op, value],
            )?
        }
        Op::Check => return Err(Unsupported::Step("opt:check")),
        // A TDZ guard: throw when the slot holds the uninitialized marker. The
        // helper sets the pending error and the body bails with `undefined`,
        // which the runtime surfaces as the `ReferenceError` (mirrors the
        // per-step `emit_tdz_check`).
        Op::TdzCheck => {
            let Imm::Slot(slot) = &inst.imm else {
                return Err(Unsupported::Step("opt:tdz"));
            };
            let bits = builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                abi.frame,
                Offset32::new((*slot as i32) * 8),
            );
            let is_uninit = builder.ins().icmp_imm_u(
                IntCC::Equal,
                bits,
                crux::value::UNINITIALIZED_BITS as i64,
            );
            let throw = builder.create_block();
            let cont = builder.create_block();
            builder.ins().brif(is_uninit, throw, &[], cont, &[]);
            builder.switch_to_block(throw);
            let f = helpers
                .get(Helper::TdzError)
                .ok_or(Unsupported::Helper(Helper::TdzError.name()))?;
            let callee = builder.ins().iconst(types::I64, f as i64);
            builder.ins().call_indirect(abi.sig_tdz, callee, &[abi.vm]);
            let undef = builder
                .ins()
                .iconst(types::I64, JsValue::Undefined.bits() as i64);
            builder.ins().return_(&[undef]);
            builder.seal_block(throw);
            builder.switch_to_block(cont);
            builder.seal_block(cont);
            builder
                .ins()
                .iconst(types::I64, JsValue::Undefined.bits() as i64)
        }
        Op::CompletionReset => {
            let undefined = builder
                .ins()
                .iconst(types::I64, JsValue::Undefined.bits() as i64);
            emit_completion_store(builder, abi.vm, undefined, true);
            undefined
        }
        Op::CompletionStore => {
            let value = arg(0)?;
            emit_completion_store(builder, abi.vm, value, false);
            value
        }
        // A lifted `o.x`: the same `get_member_name` helper the per-step path
        // calls on its slow path, so an optimizing body and a per-step one mean
        // the same thing at the call boundary.
        Op::MemberLoad => {
            let Imm::Atom(atom) = &inst.imm else {
                return Err(Unsupported::Step("opt:member"));
            };
            let object = arg(0)?;
            let name = builder.ins().iconst(types::I64, *atom as i64);
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_unary,
                Helper::GetMemberName,
                &[object, name],
            )?
        }
        // A lifted `o[k]`: the `get_member_computed` helper.
        Op::ElementLoad => {
            let object = arg(0)?;
            let key = arg(1)?;
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_unary,
                Helper::GetMemberComputed,
                &[object, key],
            )?
        }
        // A lifted `LoadGlobal`: the same `get_global` helper as the per-step
        // global fast path's miss block.
        Op::GlobalLoad => {
            let Imm::Atom(atom) = &inst.imm else {
                return Err(Unsupported::Step("opt:global"));
            };
            let name = builder.ins().iconst(types::I64, *atom as i64);
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_bool,
                Helper::GetGlobal,
                &[name],
            )?
        }
        _ => return Err(Unsupported::Step("opt:op")),
    })
}

/// Emit a helper call with the pending-error ABI: with no try machinery a
/// helper that hit an interpreter error bails the body with `undefined` (the
/// runtime surfaces the stored error). Mirrors `call_slow`'s non-try path.
fn call_helper(
    builder: &mut FunctionBuilder,
    helpers: &JitHelpers,
    abi: &Abi,
    sig: SigRef,
    helper: Helper,
    args: &[ClifValue],
) -> Result<ClifValue, Unsupported> {
    let f = helpers
        .get(helper)
        .ok_or(Unsupported::Helper(helper.name()))?;
    let callee = builder.ins().iconst(types::I64, f as i64);
    let mut all = Vec::with_capacity(args.len() + 1);
    all.push(abi.vm);
    all.extend_from_slice(args);
    let call = builder.ins().call_indirect(sig, callee, &all);
    let result = builder.func.dfg.inst_results(call)[0];

    let pending = builder
        .ins()
        .load(types::I8, MemFlagsData::new(), abi.vm, Offset32::new(0));
    let ok = builder.ins().icmp_imm_u(IntCC::Equal, pending, 0);
    let cont = builder.create_block();
    let err = builder.create_block();
    builder.ins().brif(ok, cont, &[], err, &[]);
    builder.switch_to_block(err);
    let undef = builder
        .ins()
        .iconst(types::I64, JsValue::Undefined.bits() as i64);
    builder.ins().return_(&[undef]);
    builder.seal_block(err);
    builder.switch_to_block(cont);
    // A helper that can re-enter the interpreter may disturb the Vm state the
    // leaf-call probe's cached verdicts assume stable.
    if helper.disturbs_leaf_eligibility() {
        bump_leaf_epoch(builder, abi.vm);
    }
    Ok(result)
}

/// Write the interpreter's completion register (`Vm::completion` plus
/// `completion_is_empty`) from lowered code, mirroring the per-step lowerer's
/// `emit_completion_store`. The `Vm` pointer is loaded from the ctx here (not
/// hoisted) so the entry block stays pristine — the I2a lowerer re-switches to
/// it, and Cranelift requires a re-switched block to be untouched. The write is
/// skipped when the ctx's `vm` pointer is null — the scaffold's bare-ctx test
/// harness (which never dereferences it) calls the compiled code with a null
/// `vm`.
fn emit_completion_store(
    builder: &mut FunctionBuilder,
    ctx: ClifValue,
    value: ClifValue,
    is_empty: bool,
) {
    let vm = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        ctx,
        Offset32::new(std::mem::offset_of!(JitCallContext, vm) as i32),
    );
    let has_vm = builder.ins().icmp_imm_u(IntCC::NotEqual, vm, 0);
    let store = builder.create_block();
    let skip = builder.create_block();
    builder.ins().brif(has_vm, store, &[], skip, &[]);
    builder.switch_to_block(store);
    builder.ins().store(
        MemFlagsData::new(),
        value,
        vm,
        Offset32::new(VM_COMPLETION_OFFSET as i32),
    );
    let empty = builder.ins().iconst(types::I8, is_empty as i64);
    builder.ins().store(
        MemFlagsData::new(),
        empty,
        vm,
        Offset32::new(VM_COMPLETION_IS_EMPTY_OFFSET as i32),
    );
    builder.ins().jump(skip, &[]);
    builder.seal_block(store);
    builder.switch_to_block(skip);
    builder.seal_block(skip);
}

/// The bare f64 form of an arithmetic or ordering op whose operands are proven
/// Numbers (the `narrow` pass): a canonical Number's bits bit-cast to its f64,
/// so no tag check and no fallback are needed. A comparison yields a Boolean.
fn emit_bare_numeric(
    builder: &mut FunctionBuilder,
    op: Op,
    lhs: ClifValue,
    rhs: ClifValue,
) -> ClifValue {
    let l = builder.ins().bitcast(types::F64, MemFlagsData::new(), lhs);
    let r = builder.ins().bitcast(types::F64, MemFlagsData::new(), rhs);
    match op {
        Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            let cc = match op {
                Op::Lt => FloatCC::LessThan,
                Op::Le => FloatCC::LessThanOrEqual,
                Op::Gt => FloatCC::GreaterThan,
                _ => FloatCC::GreaterThanOrEqual,
            };
            let c = builder.ins().fcmp(cc, l, r);
            let t = builder
                .ins()
                .iconst(types::I64, JsValue::Boolean(true).bits() as i64);
            let f = builder
                .ins()
                .iconst(types::I64, JsValue::Boolean(false).bits() as i64);
            builder.ins().select(c, t, f)
        }
        _ => {
            let res = match op {
                Op::Add => builder.ins().fadd(l, r),
                Op::Sub => builder.ins().fsub(l, r),
                Op::Mul => builder.ins().fmul(l, r),
                _ => builder.ins().fdiv(l, r),
            };
            let bits = builder.ins().bitcast(types::I64, MemFlagsData::new(), res);
            canon_double(builder, bits)
        }
    }
}

/// The inline numeric fast path for `+`/`-`/`*`/`/` and the ordering
/// comparisons: when both operands are Numbers (the NaN-boxed double tag),
/// compute in f64 registers with no helper call and no interpreter round-trip;
/// otherwise fall through to `BinarySlow`. Mirrors the per-step lowerer's
/// `emit_binary_known` float path (a Number's bits bit-cast to its f64 and the
/// result canonicalized against the tag region), so an optimizing body's hot
/// arithmetic costs what a per-step body's does.
fn call_numeric_binary(
    builder: &mut FunctionBuilder,
    helpers: &JitHelpers,
    abi: &Abi,
    op: Op,
    disc: i64,
    lhs: ClifValue,
    rhs: ClifValue,
) -> Result<ClifValue, Unsupported> {
    let lhs_num = builder.ins().bitcast(types::F64, MemFlagsData::new(), lhs);
    let rhs_num = builder.ins().bitcast(types::F64, MemFlagsData::new(), rhs);
    let fast = match op {
        Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            let cc = match op {
                Op::Lt => FloatCC::LessThan,
                Op::Le => FloatCC::LessThanOrEqual,
                Op::Gt => FloatCC::GreaterThan,
                _ => FloatCC::GreaterThanOrEqual,
            };
            let c = builder.ins().fcmp(cc, lhs_num, rhs_num);
            let t = builder
                .ins()
                .iconst(types::I64, JsValue::Boolean(true).bits() as i64);
            let f = builder
                .ins()
                .iconst(types::I64, JsValue::Boolean(false).bits() as i64);
            builder.ins().select(c, t, f)
        }
        _ => {
            let res = match op {
                Op::Add => builder.ins().fadd(lhs_num, rhs_num),
                Op::Sub => builder.ins().fsub(lhs_num, rhs_num),
                Op::Mul => builder.ins().fmul(lhs_num, rhs_num),
                _ => builder.ins().fdiv(lhs_num, rhs_num),
            };
            let bits = builder.ins().bitcast(types::I64, MemFlagsData::new(), res);
            canon_double(builder, bits)
        }
    };
    let lhs_dbl = is_double(builder, lhs);
    let rhs_dbl = is_double(builder, rhs);
    let both = builder.ins().band(lhs_dbl, rhs_dbl);
    let result = builder.declare_var(types::I64);
    builder.def_var(result, fast);
    let merge = builder.create_block();
    let slow = builder.create_block();
    builder.ins().brif(both, merge, &[], slow, &[]);
    builder.switch_to_block(slow);
    let op_imm = builder.ins().iconst(types::I64, disc);
    let slow_res = call_helper(
        builder,
        helpers,
        abi,
        abi.sig_binary,
        Helper::BinarySlow,
        &[op_imm, lhs, rhs],
    )?;
    builder.def_var(result, slow_res);
    builder.ins().jump(merge, &[]);
    builder.seal_block(slow);
    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.use_var(result))
}

/// `bits & TAG_MASK != TAG_PREFIX` — the Number (double) tag check.
fn is_double(builder: &mut FunctionBuilder, bits: ClifValue) -> ClifValue {
    let masked = builder.ins().band_imm_u(bits, crux::TAG_MASK as i64);
    builder
        .ins()
        .icmp_imm_u(IntCC::NotEqual, masked, crux::TAG_PREFIX as i64)
}

/// Canonicalize a computed double's bits: a NaN whose top bits collide with the
/// tag region would read as a tag, so replace it with the canonical NaN (the
/// same normalization `Value::Number` does).
fn canon_double(builder: &mut FunctionBuilder, bits: ClifValue) -> ClifValue {
    let masked = builder.ins().band_imm_u(bits, crux::TAG_MASK as i64);
    let collides = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, masked, crux::TAG_PREFIX as i64);
    let canon = builder
        .ins()
        .iconst(types::I64, JsValue::Number(f64::NAN).bits() as i64);
    builder.ins().select(collides, canon, bits)
}

/// The inline integer fast path for the bitwise/shift ops: when both operands
/// carry the double tag and lie inside the `ToInt32` conversion's range,
/// truncate to i32, apply the bit op and convert back; otherwise fall through to
/// `BinarySlow`. Mirrors the per-step lowerer's `emit_int_binary` + `trunc_i32`
/// (outside `|x| < 2^63` the saturating conversion's low bits differ from the
/// spec's wrap-around, so the range guard is required).
fn call_int_binary(
    builder: &mut FunctionBuilder,
    helpers: &JitHelpers,
    abi: &Abi,
    op: Op,
    disc: i64,
    lhs: ClifValue,
    rhs: ClifValue,
) -> Result<ClifValue, Unsupported> {
    let lhs_num = builder.ins().bitcast(types::F64, MemFlagsData::new(), lhs);
    let rhs_num = builder.ins().bitcast(types::F64, MemFlagsData::new(), rhs);
    let (l_wide, l_range) = trunc_i32(builder, lhs_num);
    let (r_wide, r_range) = trunc_i32(builder, rhs_num);
    let l = builder.ins().ireduce(types::I32, l_wide);
    let r = builder.ins().ireduce(types::I32, r_wide);
    // `ishl`/`sshr`/`ushr` mask the amount to the operand size, which is the
    // spec's own shift-count reduction.
    let (res, unsigned) = match op {
        Op::BitAnd => (builder.ins().band(l, r), false),
        Op::BitXor => (builder.ins().bxor(l, r), false),
        Op::BitOr => (builder.ins().bor(l, r), false),
        Op::Shl => (builder.ins().ishl(l, r), false),
        Op::Shr => (builder.ins().sshr(l, r), false),
        _ => (builder.ins().ushr(l, r), true),
    };
    // `>>>` answers a `ToUint32`; the others a signed i32.
    let res_f = if unsigned {
        let wide = builder.ins().uextend(types::I64, res);
        builder.ins().fcvt_from_uint(types::F64, wide)
    } else {
        let wide = builder.ins().sextend(types::I64, res);
        builder.ins().fcvt_from_sint(types::F64, wide)
    };
    let bits = builder
        .ins()
        .bitcast(types::I64, MemFlagsData::new(), res_f);
    let fast = canon_double(builder, bits);
    let lhs_dbl = is_double(builder, lhs);
    let rhs_dbl = is_double(builder, rhs);
    let both = builder.ins().band(lhs_dbl, rhs_dbl);
    let in_range = builder.ins().band(l_range, r_range);
    let both = builder.ins().band(both, in_range);
    let result = builder.declare_var(types::I64);
    builder.def_var(result, fast);
    let merge = builder.create_block();
    let slow = builder.create_block();
    builder.ins().brif(both, merge, &[], slow, &[]);
    builder.switch_to_block(slow);
    let op_imm = builder.ins().iconst(types::I64, disc);
    let slow_res = call_helper(
        builder,
        helpers,
        abi,
        abi.sig_binary,
        Helper::BinarySlow,
        &[op_imm, lhs, rhs],
    )?;
    builder.def_var(result, slow_res);
    builder.ins().jump(merge, &[]);
    builder.seal_block(slow);
    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.use_var(result))
}

/// Truncate an f64 toward zero to i64 and report whether it is inside the
/// conversion's range (`|x| < 2^63`).
fn trunc_i32(builder: &mut FunctionBuilder, num: ClifValue) -> (ClifValue, ClifValue) {
    let magnitude = builder.ins().fabs(num);
    let bound = builder.ins().f64const(9223372036854775808.0);
    let in_range = builder.ins().fcmp(FloatCC::LessThan, magnitude, bound);
    let int = builder.ins().fcvt_to_sint_sat(types::I64, num);
    (int, in_range)
}

fn bump_leaf_epoch(builder: &mut FunctionBuilder, vm: ClifValue) {
    let offset = Offset32::new(std::mem::offset_of!(JitCallContext, leaf_epoch) as i32);
    let epoch = builder
        .ins()
        .load(types::I32, MemFlagsData::new(), vm, offset);
    let bumped = builder.ins().iadd_imm_u(epoch, 1);
    builder.ins().store(MemFlagsData::new(), bumped, vm, offset);
}

fn value_of(values: &[Option<ClifValue>], id: ValueId) -> Result<ClifValue, Unsupported> {
    values
        .get(id as usize)
        .copied()
        .flatten()
        .ok_or(Unsupported::Step("opt:use-before-def"))
}

fn binary_op(op: Op) -> Option<BinaryOp> {
    Some(match op {
        Op::Pow => BinaryOp::Exp,
        Op::Mul => BinaryOp::Mul,
        Op::Div => BinaryOp::Div,
        Op::Mod => BinaryOp::Rem,
        Op::Add => BinaryOp::Add,
        Op::Sub => BinaryOp::Sub,
        Op::Shl => BinaryOp::LeftShift,
        Op::Shr => BinaryOp::RightShift,
        Op::UShr => BinaryOp::UnsignedRightShift,
        Op::Lt => BinaryOp::LessThan,
        Op::Gt => BinaryOp::GreaterThan,
        Op::Le => BinaryOp::LessEqual,
        Op::Ge => BinaryOp::GreaterEqual,
        Op::Eq => BinaryOp::Equal,
        Op::StrictEq => BinaryOp::StrictEqual,
        Op::BitAnd => BinaryOp::BitAnd,
        Op::BitXor => BinaryOp::BitXor,
        Op::BitOr => BinaryOp::BitOr,
        _ => return None,
    })
}

fn unary_op(op: Op) -> Option<UnaryOp> {
    Some(match op {
        Op::ToNumber => UnaryOp::Plus,
        Op::Neg => UnaryOp::Minus,
        Op::BitNot => UnaryOp::BitNot,
        Op::Not => UnaryOp::Not,
        _ => return None,
    })
}
