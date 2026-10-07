//! The IR -> Cranelift lowering (`.notes/optimizing-tier-impl.md` §4).
//!
//! Increment I1: an identity lowering of the lifted straight-line IR, reusing
//! the helper table and the `JitCallContext` ABI unchanged, so a body lowered
//! here is interchangeable with a per-step one at a call boundary. Gated by
//! `SLAG_OPT` (see [`JitEngine`](crate::JitEngine)); a body `opt::lift` refuses
//! — or that this lowerer then declines — keeps the per-step path.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::immediates::Offset32;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, InstBuilder, MemFlagsData, SigRef, UserFuncName, Value as ClifValue,
    types,
};
use cranelift_codegen::isa::{CallConv, TargetIsa};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use crux::Value as JsValue;
use runtime::jit::JitCallContext;
use syntax::ast::{BinaryOp, UnaryOp};

use crate::Compiled;
use crate::compiler::{Unsupported, assemble, helper_sig, jit_sig, platform_call_conv};
use crate::helpers::{Helper, JitHelpers};
use crate::opt::ir::{BlockId, Function as IrFunction, Imm, Inst, Op, Term, ValueId};

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
}

fn lower(
    ir: &IrFunction,
    helpers: &JitHelpers,
    func: &mut Function,
    fctx: &mut FunctionBuilderContext,
    conv: CallConv,
) -> Result<(), Unsupported> {
    let mut builder = FunctionBuilder::new(func, fctx);
    // One Cranelift block per IR block. The entry takes the function
    // parameters; every other block takes its IR parameters (the join stack).
    let mut clif: Vec<Block> = Vec::with_capacity(ir.block_count());
    let entry = builder.create_block();
    builder.append_block_params_for_function_params(entry);
    clif.push(entry);
    for b in 1..ir.block_count() as BlockId {
        let block = builder.create_block();
        for _ in 0..ir.block(b).params.len() {
            builder.append_block_param(block, types::I64);
        }
        clif.push(block);
    }

    builder.switch_to_block(clif[0]);
    let entry_params = builder.block_params(clif[0]).to_vec();
    let abi = Abi {
        frame: entry_params[0],
        vm: entry_params[2],
        sig_binary: builder.import_signature(helper_sig(&[types::I64; 4], conv)),
        sig_unary: builder.import_signature(helper_sig(&[types::I64; 3], conv)),
        sig_bool: builder.import_signature(helper_sig(&[types::I64; 2], conv)),
    };
    let undefined = JsValue::Undefined.bits() as i64;

    let mut values: Vec<Option<ClifValue>> = vec![None; ir.value_count() as usize];
    for b in 0..ir.block_count() as BlockId {
        let block = ir.block(b);
        builder.switch_to_block(clif[b as usize]);
        let params = builder.block_params(clif[b as usize]).to_vec();
        for (i, p) in block.params.iter().enumerate() {
            values[*p as usize] = Some(params[i]);
        }
        for inst in &block.insts {
            let result = lower_inst(&mut builder, inst, &abi, helpers, &values)?;
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
            let op = builder.ins().iconst(types::I64, disc as i64);
            let (lhs, rhs) = (arg(0)?, arg(1)?);
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_binary,
                Helper::BinarySlow,
                &[op, lhs, rhs],
            )?
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
