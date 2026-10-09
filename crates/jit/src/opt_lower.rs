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
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use crux::Value as JsValue;
use runtime::ir::{GLOBAL_CELLS, INTRINSICS, Intrinsic};
use runtime::jit::{
    DISPATCH_DEOPT, GlobalValueCell, JitCallContext, VM_COMPLETION_IS_EMPTY_OFFSET,
    VM_COMPLETION_OFFSET, VM_IP_OFFSET,
};
use syntax::ast::{BinaryOp, UnaryOp};

use crate::Compiled;
use crate::compiler::{Unsupported, assemble, helper_sig, jit_sig, platform_call_conv};
use crate::helpers::{Helper, JitHelpers};
use crate::opt::ir::{BlockId, Function as IrFunction, Imm, Inst, Op, Term, Type, ValueId};

/// How many bodies the optimizing tier has lowered (test introspection).
#[cfg(test)]
pub(crate) static OPT_COMPILED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The number of `Op::Call` instructions the identity lowering has emitted
/// (test introspection: a calling body lowered through the tier).
#[cfg(test)]
pub(crate) static OPT_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The number of `Op::Intrinsic` instructions the lowering has emitted (test
/// introspection: a body with an intrinsic lowered through the tier).
#[cfg(test)]
pub(crate) static OPT_INTRINSICS: std::sync::atomic::AtomicUsize =
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
    let compiled = assemble(isa, func, max_stack, ir_has_deopt(ir))?;
    #[cfg(test)]
    OPT_COMPILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(compiled)
}

/// The entry ABI a lowered op reads: the frame base, the working-region base,
/// the `JitCallContext` pointer, and the shared helper signatures.
struct Abi {
    frame: ClifValue,
    /// The working-region base handed to the compiled body (the second
    /// parameter of the entry signature). A guard mirrors the operand stack
    /// here before it resumes the interpreter.
    work: ClifValue,
    vm: ClifValue,
    sig_binary: SigRef,
    sig_unary: SigRef,
    sig_bool: SigRef,
    sig_tdz: SigRef,
    /// The `(vm, callee, this, argc, args, direct_eval)` signature of the
    /// general `call_slow` helper.
    sig_call_slow: SigRef,
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
        work: entry_params[1],
        vm: entry_params[2],
        sig_binary: builder.import_signature(helper_sig(&[types::I64; 4], conv)),
        sig_unary: builder.import_signature(helper_sig(&[types::I64; 3], conv)),
        sig_bool: builder.import_signature(helper_sig(&[types::I64; 2], conv)),
        sig_tdz: builder.import_signature(helper_sig(&[types::I64; 1], conv)),
        sig_call_slow: builder.import_signature(helper_sig(&[types::I64; 6], conv)),
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
                // A `Bool`-typed condition is already a canonical boolean, so
                // the truthiness test is a compare against `false` — no helper
                // call and no interpreter round-trip per branch (the loop test).
                let test = if ir.value_type(*cond) == Type::Bool {
                    builder.ins().icmp_imm_u(
                        IntCC::NotEqual,
                        c,
                        JsValue::Boolean(false).bits() as i64,
                    )
                } else {
                    let truthy = call_helper(
                        &mut builder,
                        helpers,
                        &abi,
                        abi.sig_bool,
                        Helper::ToBooleanSlow,
                        &[c],
                    )?;
                    builder.ins().icmp_imm_u(IntCC::NotEqual, truthy, 0)
                };
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

/// Whether the lowered IR contains a speculation guard, so the machine code can
/// return `DISPATCH_DEOPT` (and must not be inlined by a compiled caller).
fn ir_has_deopt(ir: &IrFunction) -> bool {
    (0..ir.block_count() as BlockId).any(|b| {
        ir.block(b)
            .insts
            .iter()
            .any(|inst| matches!(inst.op, Op::GuardType | Op::Check))
    })
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
                // Raw NaN-boxed bits (a boxed `Value` constant, e.g. a callee
                // guard's expected function identity).
                Imm::U64(v) => *v as i64,
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
            // with a known-number operand. When only one side is proven, the
            // checked path still skips that side's tag check.
            let known_lhs = types
                .get(inst.args[0] as usize)
                .is_some_and(|t| matches!(t, Type::Number | Type::Int));
            let known_rhs = types
                .get(inst.args[1] as usize)
                .is_some_and(|t| matches!(t, Type::Number | Type::Int));
            let both_known = known_lhs && known_rhs;
            let known = numeric && both_known;
            if known {
                emit_bare_numeric(builder, inst.op, lhs, rhs)
            } else if numeric {
                call_numeric_binary(
                    builder,
                    helpers,
                    abi,
                    inst.op,
                    disc as i64,
                    (lhs, rhs, (known_lhs, known_rhs)),
                )?
            } else if inst.op == Op::Mod {
                // `%` needs the integer fast path the arithmetic ops do not: a
                // proven Number still has to satisfy the integrality, i32-range
                // and nonzero-divisor guards before `srem` is the spec's `fmod`.
                call_mod_int(builder, helpers, abi, disc as i64, lhs, rhs, both_known)?
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
        // A speculation guard. `args[0]` is the condition (exit when falsy),
        // `args[1..]` the live operand stack (bottom to top), and `imm` the
        // step to resume at. The deopt block mirrors the stack into the working
        // region, points the context and `vm.ip` at the resume step, and
        // returns `DISPATCH_DEOPT`; `run_jit_body` rebuilds `vm.stack` from the
        // region and the interpreter re-executes the step (see
        // `.notes/tier-resume-fidelity.md`). The continuation is a separate
        // block, so a guard may sit mid-block with the CFG intact.
        // The continuation is a separate block, so a guard may sit mid-block
        // with the CFG intact.
        Op::Check => {
            let Imm::Int(step) = inst.imm else {
                return Err(Unsupported::Step("opt:check"));
            };
            let cond = arg(0)?;
            let truthy = if types.get(inst.args[0] as usize) == Some(&Type::Bool) {
                builder.ins().icmp_imm_u(
                    IntCC::NotEqual,
                    cond,
                    JsValue::Boolean(false).bits() as i64,
                )
            } else {
                let t = call_helper(
                    builder,
                    helpers,
                    abi,
                    abi.sig_bool,
                    Helper::ToBooleanSlow,
                    &[cond],
                )?;
                builder.ins().icmp_imm_u(IntCC::NotEqual, t, 0)
            };
            emit_guard(builder, abi, truthy, step, &inst.args[1..], values)?;
            builder
                .ins()
                .iconst(types::I64, JsValue::Undefined.bits() as i64)
        }
        // A type guard (`.notes/tier-typer.md` T1): `args[0]`'s runtime value
        // is asserted to have the result type, so a dominated op on it may
        // lower tag-free; the guarded value is the same bits re-typed, so the
        // success path emits no work. Only `Number` is checked for now — the
        // type the bare arithmetic path (`emit_bare_numeric`) keys on; any
        // other type is a refusal (the per-step path), never a silent guess.
        Op::GuardType => {
            let Imm::Int(step) = inst.imm else {
                return Err(Unsupported::Step("opt:guard"));
            };
            let value = arg(0)?;
            let ok = match inst.ty {
                Type::Number => is_double(builder, value),
                _ => return Err(Unsupported::Step("opt:guard-type")),
            };
            emit_guard(builder, abi, ok, step, &inst.args[1..], values)?;
            value
        }
        // A callee guard (I5c-2b): the success path is the callee value
        // unchanged; a mismatch retires the body via the same deopt block
        // `Op::GuardType` uses. `args[2..]` is the live operand stack.
        Op::GuardCallee => {
            let Imm::Int(step) = inst.imm else {
                return Err(Unsupported::Step("opt:guard-callee"));
            };
            if inst.args.len() < 2 {
                return Err(Unsupported::Step("opt:guard-callee"));
            }
            let callee = arg(0)?;
            let expected = arg(1)?;
            let ok = builder.ins().icmp(IntCC::Equal, callee, expected);
            emit_guard(builder, abi, ok, step, &inst.args[2..], values)?;
            callee
        }
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
        // A speculative member-value cell load (see `crate::cells`): the cell's
        // value for `(object.id, atom)`, loaded behind an object-tag check so a
        // non-Object receiver never dereferences. The value is only used under a
        // covering `Op::MemberGuard`, which re-validates the cell and its value.
        Op::MemberCellLoad => {
            let Imm::Atom(atom) = &inst.imm else {
                return Err(Unsupported::Step("opt:member-cell"));
            };
            let object = arg(0)?;
            let name = builder.ins().iconst(types::I64, *atom as i64);
            let cells = builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                abi.vm,
                Offset32::new(std::mem::offset_of!(JitCallContext, member_value_cells) as i32),
            );
            let result = builder.declare_var(types::I64);
            let dummy = builder
                .ins()
                .iconst(types::I64, JsValue::Undefined.bits() as i64);
            builder.def_var(result, dummy);
            let load = builder.create_block();
            let merge = builder.create_block();
            let obj_ok = crate::cells::is_plain_object(builder, object);
            builder.ins().brif(obj_ok, load, &[], merge, &[]);
            builder.switch_to_block(load);
            let ptr = crate::cells::object_data_ptr(builder, object);
            let live_id = builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                ptr,
                Offset32::new(std::mem::offset_of!(crux::JsObject, id) as i32),
            );
            let cell = crate::cells::member_value_cell_addr(builder, cells, live_id, name);
            let value = crate::cells::member_value_cell_value(builder, cell);
            builder.def_var(result, value);
            builder.ins().jump(merge, &[]);
            builder.seal_block(load);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.use_var(result)
        }
        // The validity guard for a preceding `Op::MemberCellLoad`: when the cell
        // still matches `object`'s live id, name and generation and holds the
        // speculative value (arg 1), that value is the read; otherwise the full
        // `get_member_name` helper serves it. Never traps.
        Op::MemberGuard => {
            let Imm::Atom(atom) = &inst.imm else {
                return Err(Unsupported::Step("opt:member-guard"));
            };
            let object = arg(0)?;
            let spec = arg(1)?;
            let name = builder.ins().iconst(types::I64, *atom as i64);
            let cells = builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                abi.vm,
                Offset32::new(std::mem::offset_of!(JitCallContext, member_value_cells) as i32),
            );
            let result = builder.declare_var(types::I64);
            let check = builder.create_block();
            let slow = builder.create_block();
            let merge = builder.create_block();
            let obj_ok = crate::cells::is_plain_object(builder, object);
            builder.ins().brif(obj_ok, check, &[], slow, &[]);
            builder.switch_to_block(check);
            let ptr = crate::cells::object_data_ptr(builder, object);
            let live_id = builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                ptr,
                Offset32::new(std::mem::offset_of!(crux::JsObject, id) as i32),
            );
            let live_gen = builder.ins().load(
                types::I32,
                MemFlagsData::new(),
                ptr,
                Offset32::new(std::mem::offset_of!(crux::JsObject, generation) as i32),
            );
            let cell = crate::cells::member_value_cell_addr(builder, cells, live_id, name);
            let cell_ok =
                crate::cells::member_value_cell_valid(builder, cell, live_id, *atom, live_gen);
            let cell_value = crate::cells::member_value_cell_value(builder, cell);
            let value_ok = builder.ins().icmp(IntCC::Equal, cell_value, spec);
            let ok = builder.ins().band(cell_ok, value_ok);
            builder.def_var(result, spec);
            builder.ins().brif(ok, merge, &[], slow, &[]);
            builder.seal_block(check);
            builder.switch_to_block(slow);
            let res = call_helper(
                builder,
                helpers,
                abi,
                abi.sig_unary,
                Helper::GetMemberName,
                &[object, name],
            )?;
            builder.def_var(result, res);
            builder.ins().jump(merge, &[]);
            builder.seal_block(slow);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.use_var(result)
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
        // A lifted member store (`o.x = v`): the validated store — the
        // member-value cell probe → the narrow `set_member_slot`, falling back
        // to the full `set_member_name` helper. `args = [object, value]`,
        // `Imm::Atom(name)`; effect-only.
        Op::MemberStore => {
            let Imm::Atom(atom) = &inst.imm else {
                return Err(Unsupported::Step("opt:member-store"));
            };
            let object = arg(0)?;
            let value = arg(1)?;
            emit_member_store(builder, helpers, abi, object, value, *atom)?;
            builder
                .ins()
                .iconst(types::I64, JsValue::Undefined.bits() as i64)
        }
        // A lifted `o[k]`: the inline dense element read (the read mirror of
        // the append), falling back to `get_member_computed` for every other
        // receiver, key, or a hole.
        Op::ElementLoad => {
            let object = arg(0)?;
            let key = arg(1)?;
            emit_element_read(builder, helpers, abi, object, key)?
        }
        // A lifted `a[k] = v`: the authoritative computed store helper. `args =
        // [object, key, value]`; effect-only.
        Op::ElementStore => {
            let object = arg(0)?;
            let key = arg(1)?;
            let value = arg(2)?;
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_binary,
                Helper::SetMemberComputed,
                &[object, key, value],
            )?;
            builder
                .ins()
                .iconst(types::I64, JsValue::Undefined.bits() as i64)
        }
        // A lifted `LoadGlobal`: the inline direct-mapped global-value cell (the
        // read mirror of the per-step `emit_global_read`), falling back to the
        // `get_global` helper on a miss.
        Op::GlobalLoad => {
            let Imm::Atom(atom) = &inst.imm else {
                return Err(Unsupported::Step("opt:global"));
            };
            emit_global_read(builder, helpers, abi, *atom, false, Helper::GetGlobal)?
        }
        // A lifted `LoadIdent` (an env-resolved global read, `BindingLoc::Env`):
        // the same inline cell probe, gated on the ctx's `globals_unshadowed` (a
        // hit returns the GLOBAL binding's value, so the body's own reads must not
        // be shadowed by its chain), falling back to the `load_ident` helper.
        Op::IdentLoad => {
            let Imm::Atom(atom) = &inst.imm else {
                return Err(Unsupported::Step("opt:ident"));
            };
            emit_global_read(builder, helpers, abi, *atom, true, Helper::LoadIdent)?
        }
        // A general call (I5c-0): `args = [this, callee, a1..aN]`. The identity
        // lowering materializes the arguments at the working-region base and
        // runs the general `call_slow` helper, so a calling body enters the
        // tier; a later slice (I5c-2) splices the callee in place of the call.
        Op::Call => {
            if inst.args.len() < 2 {
                return Err(Unsupported::Step("opt:call"));
            }
            #[cfg(test)]
            OPT_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let this = arg(0)?;
            let callee = arg(1)?;
            let argc = inst.args.len() - 2;
            for k in 0..argc {
                let a = arg(2 + k)?;
                builder.ins().store(
                    MemFlagsData::new(),
                    a,
                    abi.work,
                    Offset32::new((k * 8) as i32),
                );
            }
            let argc_imm = builder.ins().iconst(types::I64, argc as i64);
            let dir_imm = builder.ins().iconst(types::I64, 0);
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_call_slow,
                Helper::CallSlow,
                &[callee, this, argc_imm, abi.work, dir_imm],
            )?
        }
        // A lifted `x.f(a, ...)` intrinsic (I4a): `args = [this, callee, a1..aN]`,
        // `imm` the `Intrinsic` discriminant. The lowering reproduces the
        // per-step fast path (the `%`-identity gate, an inline `Math` op or a
        // narrow helper) and falls back to the general call.
        Op::Intrinsic => {
            let Imm::Int(disc) = inst.imm else {
                return Err(Unsupported::Step("opt:intrinsic"));
            };
            if inst.args.len() < 2 {
                return Err(Unsupported::Step("opt:intrinsic"));
            }
            let kind = INTRINSICS
                .get(disc as usize)
                .copied()
                .ok_or(Unsupported::Step("opt:intrinsic"))?;
            #[cfg(test)]
            OPT_INTRINSICS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let this = arg(0)?;
            let callee = arg(1)?;
            let mut call_args = Vec::with_capacity(inst.args.len() - 2);
            for i in 2..inst.args.len() {
                call_args.push(arg(i)?);
            }
            emit_intrinsic(builder, helpers, abi, kind, this, callee, &call_args)?
        }
        // A lifted whole-literal array create (`Step::ArrayFast`): the args are
        // the element values in source order. Materialize them at the working
        // region base and run the same `array_fast` helper the per-step path
        // uses (I6-0a).
        Op::NewArray => {
            let mut values = Vec::with_capacity(inst.args.len());
            for i in 0..inst.args.len() {
                values.push(arg(i)?);
            }
            emit_new_array(builder, helpers, abi, &values)?
        }
        // A lifted whole-literal object create (`Step::ObjectFast`): the args are
        // the property values in source order; the `names` payload stays in the
        // running body and the helper reads it back via the step index (`imm`).
        Op::NewObject => {
            let Imm::Int(step) = inst.imm else {
                return Err(Unsupported::Step("opt:new-object"));
            };
            let mut values = Vec::with_capacity(inst.args.len());
            for i in 0..inst.args.len() {
                values.push(arg(i)?);
            }
            emit_new_object(builder, helpers, abi, step, &values)?
        }
        // The non-fused array literal steps (I6-0b): the same `array_*` helpers
        // the per-step path calls, with the container threaded as an SSA value
        // (the VM's `array_index_stack` tracks the element index between steps).
        Op::ArrayBegin => call_helper(builder, helpers, abi, abi.sig_tdz, Helper::ArrayBegin, &[])?,
        Op::ArrayElement => {
            let array = arg(0)?;
            let value = arg(1)?;
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_unary,
                Helper::ArrayElement,
                &[array, value],
            )?
        }
        Op::ArrayEnd => {
            let array = arg(0)?;
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_bool,
                Helper::ArrayEnd,
                &[array],
            )?
        }
        // The non-fused object literal steps (I6-0b). `ObjectInitName`'s payload
        // (name, set_name, shorthand) rides as the last three SSA args.
        Op::ObjectBegin => {
            call_helper(builder, helpers, abi, abi.sig_tdz, Helper::ObjectBegin, &[])?
        }
        Op::ObjectInitName => {
            let object = arg(0)?;
            let value = arg(1)?;
            let name = arg(2)?;
            let set = arg(3)?;
            let short = arg(4)?;
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_call_slow,
                Helper::ObjectInitName,
                &[object, name, set, short, value],
            )?
        }
        // The vector-call argument steps (I-vector): the same `args_*` helpers
        // the per-step path calls (the vector lives in the VM).
        Op::ArgsBase => {
            call_helper(builder, helpers, abi, abi.sig_tdz, Helper::ArgsBase, &[])?;
            builder
                .ins()
                .iconst(types::I64, JsValue::Undefined.bits() as i64)
        }
        Op::ArgsPush => {
            let value = arg(0)?;
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_bool,
                Helper::ArgsPush,
                &[value],
            )?;
            builder
                .ins()
                .iconst(types::I64, JsValue::Undefined.bits() as i64)
        }
        Op::ArgsSpread => {
            let iterable = arg(0)?;
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_bool,
                Helper::ArgsSpread,
                &[iterable],
            )?;
            builder
                .ins()
                .iconst(types::I64, JsValue::Undefined.bits() as i64)
        }
        // The vector-form construct (`[callee]` on the stack, the args in the
        // VM vector): the helper runs the construct machinery; `sp` is the
        // working-region base (a soft carve base — an out-of-room `sp` falls back
        // to `run_jit_leaf`).
        Op::Construct => {
            let callee = arg(0)?;
            call_helper(
                builder,
                helpers,
                abi,
                abi.sig_unary,
                Helper::Construct,
                &[callee, abi.work],
            )?
        }
        _ => return Err(Unsupported::Step("opt:op")),
    })
}

/// The inline computed element read: a dense Array's canonical-index element or
/// a numeric TypedArray element, declining to `get_member_computed` for any
/// other receiver, key, or shape. Ports the per-step `emit_element_read`'s dense
/// and typed arms (`.agents/skills/slag-dense-arrays` §8).
fn emit_element_read(
    builder: &mut FunctionBuilder,
    helpers: &JitHelpers,
    abi: &Abi,
    object: ClifValue,
    key: ClifValue,
) -> Result<ClifValue, Unsupported> {
    let value = builder.declare_var(types::I64);
    let kind = builder.create_block();
    let dense = builder.create_block();
    let typed = builder.create_block();
    let slow = builder.create_block();
    let merge = builder.create_block();

    // Object tag: the top 20 bits equal the packed prefix+Object pattern, so
    // one compare checks the heap prefix and the Object tag.
    let object_pattern = (crux::TAG_PREFIX >> 44) | crux::TAG_OBJECT;
    let tag_bits = builder.ins().ushr_imm_u(object, 44);
    let obj_ok = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, tag_bits, object_pattern as i64);
    builder.ins().brif(obj_ok, kind, &[], slow, &[]);
    builder.seal_block(kind);

    // The receiver pointer and the dense cursor: `array_dense` is non-null iff
    // the dense representation is active (a spill clears it), so it IS the
    // dense switch; otherwise the object may be a numeric TypedArray.
    builder.switch_to_block(kind);
    let obj_ptr = builder.ins().band_imm_u(object, crux::PAYLOAD_MASK as i64);
    let obj_ptr = builder.ins().ishl_imm_u(obj_ptr, 4);
    let obj_ptr = builder
        .ins()
        .iadd_imm_s(obj_ptr, crux::heap::GCBOX_DATA_OFFSET as i64);
    let dense_base = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        obj_ptr,
        Offset32::new(std::mem::offset_of!(crux::JsObject, array_dense) as i32),
    );
    let is_dense = builder.ins().icmp_imm_u(IntCC::NotEqual, dense_base, 0);
    builder.ins().brif(is_dense, dense, &[], typed, &[]);
    builder.seal_block(dense);
    builder.seal_block(typed);
    emit_dense_element_read(builder, dense_base, key, dense, value, slow, merge);
    emit_typed_element_read(builder, obj_ptr, key, typed, value, slow, merge);

    // The helper: the full `[[Get]]` interns the key, serves a prototype
    // element or accessor, and every other shape.
    builder.seal_block(slow);
    builder.switch_to_block(slow);
    let res = call_helper(
        builder,
        helpers,
        abi,
        abi.sig_unary,
        Helper::GetMemberComputed,
        &[object, key],
    )?;
    builder.def_var(value, res);
    builder.ins().jump(merge, &[]);

    builder.seal_block(merge);
    builder.switch_to_block(merge);
    Ok(builder.use_var(value))
}

/// The dense-Array arm of [`emit_element_read`]: the canonical-index gate, the
/// `idx < elem_len` bound, and the hole decline to `slow`. A hole is
/// spec-absent, so the chain may serve it — it must never be returned; a stored
/// `undefined` IS a real value and is served.
fn emit_dense_element_read(
    builder: &mut FunctionBuilder,
    dense_base: ClifValue,
    key: ClifValue,
    entry: Block,
    value: Variable,
    slow: Block,
    merge: Block,
) {
    let in_len = builder.create_block();
    let load_blk = builder.create_block();
    let hit = builder.create_block();

    // The canonical-index gate: `idx = ToUint64Sat(num)` round-trips exactly
    // when `num` is an integral double below 2^32-1, rejecting fractional,
    // negative, huge and non-double keys (a NaN-boxed heap value bitcasts to a
    // NaN, whose compare fails).
    builder.switch_to_block(entry);
    let num = builder.ins().bitcast(types::F64, MemFlagsData::new(), key);
    let max = builder.ins().f64const(4294967295.0);
    let lt_max = builder.ins().fcmp(FloatCC::LessThan, num, max);
    let idx = builder.ins().fcvt_to_uint_sat(types::I64, num);
    let back = builder.ins().fcvt_from_uint(types::F64, idx);
    let integral = builder.ins().fcmp(FloatCC::Equal, back, num);
    let key_ok = builder.ins().band(integral, lt_max);
    builder.ins().brif(key_ok, in_len, &[], slow, &[]);
    builder.seal_block(in_len);

    // The bounds gate: `elem_len` is the authoritative materialized length.
    builder.switch_to_block(in_len);
    let slots_ptr = builder
        .ins()
        .iadd_imm_s(dense_base, crux::heap::GCBOX_DATA_OFFSET as i64);
    let elem_len = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots_ptr,
        Offset32::new(std::mem::offset_of!(crux::object::ArraySlots, elem_len) as i32),
    );
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, idx, elem_len);
    builder.ins().brif(in_bounds, load_blk, &[], slow, &[]);
    builder.seal_block(load_blk);

    // The element load: a hole is spec-absent and must decline to the helper
    // (the chain may serve it), never be returned.
    builder.switch_to_block(load_blk);
    let elem_ptr = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots_ptr,
        Offset32::new(std::mem::offset_of!(crux::object::ArraySlots, elem_ptr) as i32),
    );
    let offset = builder.ins().ishl_imm_u(idx, 3);
    let addr = builder.ins().iadd(elem_ptr, offset);
    let bits = builder
        .ins()
        .load(types::I64, MemFlagsData::new(), addr, Offset32::new(0));
    let is_hole = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, bits, crux::value::HOLE_BITS as i64);
    builder.ins().brif(is_hole, slow, &[], hit, &[]);
    builder.seal_block(hit);
    builder.switch_to_block(hit);
    builder.def_var(value, bits);
    builder.ins().jump(merge, &[]);
}

/// The numeric-TypedArray arm of [`emit_element_read`], the read mirror of the
/// per-step `emit_typed_array_read_into`. The geometry gate is a READ gate: a
/// detached buffer covers no element (the helper returns `undefined`), a
/// resizable buffer is declined (a fixed view's `array_length` is the effective
/// length only while the buffer has not shrunk below it), and a null data
/// pointer declines before any load. `immutable` is deliberately NOT checked —
/// reads of an immutable buffer are allowed. Float16 and the BigInt kinds are
/// absent (a soft-float decode / a BigInt allocation) and decline.
fn emit_typed_element_read(
    builder: &mut FunctionBuilder,
    obj_ptr: ClifValue,
    key: ClifValue,
    entry: Block,
    value: Variable,
    slow: Block,
    merge: Block,
) {
    // (discriminant, element width in bytes, signed, float).
    const KINDS: [(u8, u8, bool, bool); 9] = [
        (crux::typed_array::ElementType::Uint8 as u8, 1, false, false),
        (
            crux::typed_array::ElementType::Uint8Clamped as u8,
            1,
            false,
            false,
        ),
        (crux::typed_array::ElementType::Int8 as u8, 1, true, false),
        (crux::typed_array::ElementType::Int16 as u8, 2, true, false),
        (
            crux::typed_array::ElementType::Uint16 as u8,
            2,
            false,
            false,
        ),
        (crux::typed_array::ElementType::Int32 as u8, 4, true, false),
        (
            crux::typed_array::ElementType::Uint32 as u8,
            4,
            false,
            false,
        ),
        (
            crux::typed_array::ElementType::Float32 as u8,
            4,
            false,
            true,
        ),
        (
            crux::typed_array::ElementType::Float64 as u8,
            8,
            false,
            true,
        ),
    ];
    builder.switch_to_block(entry);
    if crux::typed_array::WORKERS {
        builder.ins().jump(slow, &[]);
        return;
    }
    let geom = builder.create_block();
    let key_gate = builder.create_block();
    let mut tests: Vec<Block> = Vec::with_capacity(KINDS.len());
    let mut convs: Vec<Block> = Vec::with_capacity(KINDS.len());
    for _ in 0..KINDS.len() {
        tests.push(builder.create_block());
        convs.push(builder.create_block());
    }
    // The typed-array gate: the object's `typed_array` cell (the slots box base,
    // or 0 when the object is not Integer-Indexed).
    let slots_base = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        obj_ptr,
        Offset32::new(std::mem::offset_of!(crux::JsObject, typed_array) as i32),
    );
    let slots_ok = builder.ins().icmp_imm_u(IntCC::NotEqual, slots_base, 0);
    builder.ins().brif(slots_ok, geom, &[], slow, &[]);
    builder.seal_block(geom);
    builder.switch_to_block(geom);
    let slots_ptr = builder
        .ins()
        .iadd_imm_s(slots_base, crux::heap::GCBOX_DATA_OFFSET as i64);
    let etype = builder.ins().load(
        types::I8,
        MemFlagsData::new(),
        slots_ptr,
        Offset32::new(std::mem::offset_of!(crux::object::TypedArraySlots, element_type) as i32),
    );
    const STATE_OFFSET: usize = std::mem::offset_of!(crux::object::TypedArraySlots, buffer)
        + std::mem::offset_of!(crux::typed_array::SharedBuffer, state);
    const FLAGS_OFFSET: usize = std::mem::offset_of!(crux::object::TypedArraySlots, buffer)
        + std::mem::offset_of!(crux::typed_array::SharedBuffer, flags);
    let state_addr = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots_ptr,
        Offset32::new(STATE_OFFSET as i32),
    );
    let flags_addr = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots_ptr,
        Offset32::new(FLAGS_OFFSET as i32),
    );
    let data = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        state_addr,
        Offset32::new(std::mem::offset_of!(crux::typed_array::BlockState, data) as i32),
    );
    let detached = builder.ins().load(
        types::I8,
        MemFlagsData::new(),
        flags_addr,
        Offset32::new(std::mem::offset_of!(crux::typed_array::BufferFlags, detached) as i32),
    );
    let resizable = builder.ins().load(
        types::I8,
        MemFlagsData::new(),
        flags_addr,
        Offset32::new(std::mem::offset_of!(crux::typed_array::BufferFlags, resizable) as i32),
    );
    let arr_len = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots_ptr,
        Offset32::new(std::mem::offset_of!(crux::object::TypedArraySlots, array_length) as i32),
    );
    let byte_offset = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        slots_ptr,
        Offset32::new(std::mem::offset_of!(crux::object::TypedArraySlots, byte_offset) as i32),
    );
    let det_ok = builder.ins().icmp_imm_u(IntCC::Equal, detached, 0);
    let res_ok = builder.ins().icmp_imm_u(IntCC::Equal, resizable, 0);
    let has_data = builder.ins().icmp_imm_u(IntCC::NotEqual, data, 0);
    let geom_ok = builder.ins().band(det_ok, res_ok);
    let geom_ok = builder.ins().band(geom_ok, has_data);
    builder.ins().brif(geom_ok, key_gate, &[], slow, &[]);
    builder.seal_block(key_gate);
    // The key gate + bounds (the dense arm's gate, plus the element-count bound).
    builder.switch_to_block(key_gate);
    let num = builder.ins().bitcast(types::F64, MemFlagsData::new(), key);
    let max = builder.ins().f64const(4294967295.0);
    let lt_max = builder.ins().fcmp(FloatCC::LessThan, num, max);
    let idx = builder.ins().fcvt_to_uint_sat(types::I64, num);
    let back = builder.ins().fcvt_from_uint(types::F64, idx);
    let integral = builder.ins().fcmp(FloatCC::Equal, back, num);
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, idx, arr_len);
    let key_ok = builder.ins().band(integral, lt_max);
    let read_ok = builder.ins().band(key_ok, in_bounds);
    let base = builder.ins().iadd(data, byte_offset);
    builder.ins().brif(read_ok, tests[0], &[], slow, &[]);
    builder.seal_block(tests[0]);
    // The element-kind dispatch: one test per supported kind, chained; the last
    // kind's miss is `slow`.
    for i in 0..KINDS.len() {
        builder.switch_to_block(tests[i]);
        let matches = builder
            .ins()
            .icmp_imm_u(IntCC::Equal, etype, KINDS[i].0 as i64);
        let next = if i + 1 < KINDS.len() {
            tests[i + 1]
        } else {
            slow
        };
        builder.ins().brif(matches, convs[i], &[], next, &[]);
        builder.seal_block(convs[i]);
        if i + 1 < KINDS.len() {
            builder.seal_block(tests[i + 1]);
        }
    }
    for (i, &(_, width, signed, float)) in KINDS.iter().enumerate() {
        builder.switch_to_block(convs[i]);
        let shift = match width {
            1 => 0,
            2 => 1,
            4 => 2,
            _ => 3,
        };
        let scaled = if shift == 0 {
            idx
        } else {
            builder.ins().ishl_imm_u(idx, shift)
        };
        let addr = builder.ins().iadd(base, scaled);
        let bits = if float {
            emit_typed_element_float(builder, addr, width)
        } else {
            emit_typed_element_number(builder, addr, width, signed)
        };
        builder.def_var(value, bits);
        builder.ins().jump(merge, &[]);
    }
}

/// Load a numeric TypedArray element at `addr` and convert it to NaN-boxed
/// Number bits: `width` bytes, `signed` selecting sign- vs zero-extension.
fn emit_typed_element_number(
    builder: &mut FunctionBuilder,
    addr: ClifValue,
    width: u8,
    signed: bool,
) -> ClifValue {
    debug_assert!(width < 8, "8-byte integer elements are BigInt (declined)");
    let loaded = match width {
        1 => builder
            .ins()
            .load(types::I8, MemFlagsData::new(), addr, Offset32::new(0)),
        2 => builder
            .ins()
            .load(types::I16, MemFlagsData::new(), addr, Offset32::new(0)),
        _ => builder
            .ins()
            .load(types::I32, MemFlagsData::new(), addr, Offset32::new(0)),
    };
    let wide = if signed {
        builder.ins().sextend(types::I64, loaded)
    } else {
        builder.ins().uextend(types::I64, loaded)
    };
    let num = if signed {
        builder.ins().fcvt_from_sint(types::F64, wide)
    } else {
        builder.ins().fcvt_from_uint(types::F64, wide)
    };
    builder.ins().bitcast(types::I64, MemFlagsData::new(), num)
}

/// Load a Float32/Float64 TypedArray element at `addr` as NaN-boxed Number bits
/// (a Float64's stored bytes are already its Number bits; a Float32 widens).
fn emit_typed_element_float(
    builder: &mut FunctionBuilder,
    addr: ClifValue,
    width: u8,
) -> ClifValue {
    if width == 4 {
        let raw = builder
            .ins()
            .load(types::I32, MemFlagsData::new(), addr, Offset32::new(0));
        let narrow = builder.ins().bitcast(types::F32, MemFlagsData::new(), raw);
        let wide = builder.ins().fpromote(types::F64, narrow);
        builder.ins().bitcast(types::I64, MemFlagsData::new(), wide)
    } else {
        builder
            .ins()
            .load(types::I64, MemFlagsData::new(), addr, Offset32::new(0))
    }
}

/// The inline direct-mapped global-value read (the read mirror of the per-step
/// `emit_global_read_probe`): validate the cell's name and captured
/// identity/generation against the global object's LIVE id/generation, returning
/// the cached value on a hit and calling `fallback` (`get_global` for
/// `LoadGlobal`, `load_ident` for `LoadIdent`) on a miss. `gated` adds the
/// `globals_unshadowed` check an env-resolved read needs.
fn emit_global_read(
    builder: &mut FunctionBuilder,
    helpers: &JitHelpers,
    abi: &Abi,
    atom: u32,
    gated: bool,
    fallback: Helper,
) -> Result<ClifValue, Unsupported> {
    let value = builder.declare_var(types::I64);
    let probe = builder.create_block();
    let slow = builder.create_block();
    let merge = builder.create_block();

    let global = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        abi.vm,
        Offset32::new(std::mem::offset_of!(JitCallContext, global_object) as i32),
    );
    let cells = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        abi.vm,
        Offset32::new(std::mem::offset_of!(JitCallContext, global_value_cells) as i32),
    );
    let has_global = builder.ins().icmp_imm_u(IntCC::NotEqual, global, 0);
    let gate = if gated {
        let clean = builder.ins().load(
            types::I8,
            MemFlagsData::new(),
            abi.vm,
            Offset32::new(std::mem::offset_of!(JitCallContext, globals_unshadowed) as i32),
        );
        let clean_ok = builder.ins().icmp_imm_u(IntCC::NotEqual, clean, 0);
        builder.ins().band(has_global, clean_ok)
    } else {
        has_global
    };
    builder.ins().brif(gate, probe, &[], slow, &[]);
    builder.seal_block(probe);

    // The fast path: the cell's name and captured version must match the live
    // global (a stale cell, another realm's global, or a mid-run mutation all
    // miss).
    builder.switch_to_block(probe);
    let live_id = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        global,
        Offset32::new(std::mem::offset_of!(crux::JsObject, id) as i32),
    );
    let live_gen = builder.ins().load(
        types::I32,
        MemFlagsData::new(),
        global,
        Offset32::new(std::mem::offset_of!(crux::JsObject, generation) as i32),
    );
    let cell = builder.ins().iadd_imm_s(
        cells,
        ((atom as usize & (GLOBAL_CELLS - 1)) * std::mem::size_of::<GlobalValueCell>()) as i64,
    );
    let cell_name = builder.ins().load(
        types::I32,
        MemFlagsData::new(),
        cell,
        Offset32::new(std::mem::offset_of!(GlobalValueCell, name) as i32),
    );
    let cell_id = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        cell,
        Offset32::new(std::mem::offset_of!(GlobalValueCell, global_id) as i32),
    );
    let cell_gen = builder.ins().load(
        types::I32,
        MemFlagsData::new(),
        cell,
        Offset32::new(std::mem::offset_of!(GlobalValueCell, generation) as i32),
    );
    let cell_value = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        cell,
        Offset32::new(std::mem::offset_of!(GlobalValueCell, value) as i32),
    );
    let name_ok = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, cell_name, atom as i64);
    let id_ok = builder.ins().icmp(IntCC::Equal, cell_id, live_id);
    let gen_ok = builder.ins().icmp(IntCC::Equal, cell_gen, live_gen);
    let ok = builder.ins().band(name_ok, id_ok);
    let ok = builder.ins().band(ok, gen_ok);
    builder.def_var(value, cell_value);
    builder.ins().brif(ok, merge, &[], slow, &[]);

    // The helper: the full resolve (which also warms the cell for the next
    // read when the binding is a global data property).
    builder.seal_block(slow);
    builder.switch_to_block(slow);
    let name = builder.ins().iconst(types::I64, atom as i64);
    let res = call_helper(builder, helpers, abi, abi.sig_bool, fallback, &[name])?;
    builder.def_var(value, res);
    builder.ins().jump(merge, &[]);

    builder.seal_block(merge);
    builder.switch_to_block(merge);
    Ok(builder.use_var(value))
}

/// The fused whole-literal array create (I6-0a), the mirror of the per-step
/// `Step::ArrayFast`: materialize the element values at the working-region base
/// and run `array_fast`, which reads the `n` values below the `sp` passed here
/// (`work + 8n`).
fn emit_new_array(
    builder: &mut FunctionBuilder,
    helpers: &JitHelpers,
    abi: &Abi,
    values: &[ClifValue],
) -> Result<ClifValue, Unsupported> {
    let n = values.len();
    for (k, v) in values.iter().enumerate() {
        builder.ins().store(
            MemFlagsData::new(),
            *v,
            abi.work,
            Offset32::new((k * 8) as i32),
        );
    }
    let count = builder.ins().iconst(types::I64, n as i64);
    let sp = builder.ins().iadd_imm_u(abi.work, (n * 8) as i64);
    call_helper(
        builder,
        helpers,
        abi,
        abi.sig_unary,
        Helper::ArrayFast,
        &[count, sp],
    )
}

/// The fused whole-literal object create (I6-0a), the mirror of the per-step
/// `Step::ObjectFast`: the `names` payload is read back from the running body
/// via `step` (`step_at`), and the `n` property values are materialized at the
/// working-region base below the `sp` passed here (`work + 8n`).
fn emit_new_object(
    builder: &mut FunctionBuilder,
    helpers: &JitHelpers,
    abi: &Abi,
    step: i32,
    values: &[ClifValue],
) -> Result<ClifValue, Unsupported> {
    let n = values.len();
    for (k, v) in values.iter().enumerate() {
        builder.ins().store(
            MemFlagsData::new(),
            *v,
            abi.work,
            Offset32::new((k * 8) as i32),
        );
    }
    let step_imm = builder.ins().iconst(types::I64, step as i64);
    let sp = builder.ins().iadd_imm_u(abi.work, (n * 8) as i64);
    call_helper(
        builder,
        helpers,
        abi,
        abi.sig_unary,
        Helper::ObjectFast,
        &[step_imm, sp],
    )
}

/// `bits & TAG_MASK == TAG_PREFIX` and `(bits >> 44) & 0xF == TAG_STRING` — the
/// string check, mirroring the per-step `is_string` (the heap-prefix test runs
/// first: a double can carry the tag bits by coincidence).
fn is_string(builder: &mut FunctionBuilder, bits: ClifValue) -> ClifValue {
    let is_heap = builder.ins().band_imm_u(bits, crux::TAG_MASK as i64);
    let is_heap = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, is_heap, crux::TAG_PREFIX as i64);
    let tag = builder.ins().ushr_imm_u(bits, 44);
    let tag = builder.ins().band_imm_u(tag, 0xF);
    let is_str = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, tag, crux::TAG_STRING as i64);
    builder.ins().band(is_heap, is_str)
}

/// The `Step::CallIntrinsic` fast path, mirroring the per-step `emit_step` arm:
/// a `%`-identity gate on the callee (plus the shape checks each kind needs), an
/// inline `Math` op or a narrow helper with a sentinel fall-back, and the
/// general call as the fallback. `this` and `callee` are the resolved receiver
/// and callee; `args` is `a1..aN`.
fn emit_intrinsic(
    builder: &mut FunctionBuilder,
    helpers: &JitHelpers,
    abi: &Abi,
    kind: Intrinsic,
    this: ClifValue,
    callee: ClifValue,
    args: &[ClifValue],
) -> Result<ClifValue, Unsupported> {
    let value = builder.declare_var(types::I64);
    let fast = builder.create_block();
    let slow = builder.create_block();
    let merge = builder.create_block();

    let undefined = builder
        .ins()
        .iconst(types::I64, JsValue::Undefined.bits() as i64);
    let arg1 = args.first().copied().unwrap_or(undefined);

    // The gate: the callee is the realm's `%`-named intrinsic for `kind`
    // (`ctx.intrinsic_bits[kind]`, 0 when the body has none), plus the shape
    // checks each kind needs. The collection/array kinds validate their receiver
    // and index/`from` in the helper, so only the callee identity is guarded;
    // `Math` and `charCodeAt` need a Number argument, and `charCodeAt` a String
    // receiver.
    let bit_offset = std::mem::offset_of!(JitCallContext, intrinsic_bits)
        + kind as usize * std::mem::size_of::<u64>();
    let expected = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        abi.vm,
        Offset32::new(bit_offset as i32),
    );
    let non_zero = builder.ins().icmp_imm_u(IntCC::NotEqual, expected, 0);
    let callee_ok = builder.ins().icmp(IntCC::Equal, callee, expected);
    let mut gate = builder.ins().band(non_zero, callee_ok);
    match kind {
        Intrinsic::StringCharCodeAt => {
            let arg_ok = is_double(builder, arg1);
            gate = builder.ins().band(gate, arg_ok);
            let this_ok = is_string(builder, this);
            gate = builder.ins().band(gate, this_ok);
        }
        Intrinsic::MathAbs
        | Intrinsic::MathCeil
        | Intrinsic::MathFloor
        | Intrinsic::MathTrunc
        | Intrinsic::MathSqrt => {
            let arg_ok = is_double(builder, arg1);
            gate = builder.ins().band(gate, arg_ok);
        }
        Intrinsic::ArrayIndexOf
        | Intrinsic::MapGet
        | Intrinsic::SetHas
        | Intrinsic::MapSet
        | Intrinsic::ArrayAt
        | Intrinsic::ArrayIncludes
        | Intrinsic::ArrayPush => {}
    }
    builder.ins().brif(gate, fast, &[], slow, &[]);
    builder.seal_block(fast);

    // The fast path: an inline `Math` op (no helper) or a narrow helper whose
    // sentinel result routes to the general call.
    builder.switch_to_block(fast);
    match kind {
        Intrinsic::MathAbs
        | Intrinsic::MathCeil
        | Intrinsic::MathFloor
        | Intrinsic::MathTrunc
        | Intrinsic::MathSqrt => {
            let num = builder.ins().bitcast(types::F64, MemFlagsData::new(), arg1);
            let result = match kind {
                Intrinsic::MathAbs => builder.ins().fabs(num),
                Intrinsic::MathCeil => builder.ins().ceil(num),
                Intrinsic::MathFloor => builder.ins().floor(num),
                Intrinsic::MathTrunc => builder.ins().trunc(num),
                _ => builder.ins().sqrt(num),
            };
            let bits = builder
                .ins()
                .bitcast(types::I64, MemFlagsData::new(), result);
            let bits = canon_double(builder, bits);
            builder.def_var(value, bits);
            builder.ins().jump(merge, &[]);
        }
        Intrinsic::StringCharCodeAt => {
            let result = call_helper(
                builder,
                helpers,
                abi,
                abi.sig_unary,
                Helper::CharCodeAt,
                &[this, arg1],
            )?;
            builder.def_var(value, result);
            builder.ins().jump(merge, &[]);
        }
        _ => {
            let result = match kind {
                Intrinsic::ArrayIndexOf => {
                    let from = args.get(1).copied().unwrap_or(undefined);
                    call_helper(
                        builder,
                        helpers,
                        abi,
                        abi.sig_binary,
                        Helper::ArrayIndexOf,
                        &[this, arg1, from],
                    )?
                }
                Intrinsic::MapGet | Intrinsic::SetHas => {
                    let helper = if kind == Intrinsic::SetHas {
                        Helper::SetHas
                    } else {
                        Helper::MapGet
                    };
                    call_helper(builder, helpers, abi, abi.sig_unary, helper, &[this, arg1])?
                }
                Intrinsic::MapSet => {
                    let v = args.get(1).copied().unwrap_or(undefined);
                    call_helper(
                        builder,
                        helpers,
                        abi,
                        abi.sig_binary,
                        Helper::MapSet,
                        &[this, arg1, v],
                    )?
                }
                Intrinsic::ArrayAt => call_helper(
                    builder,
                    helpers,
                    abi,
                    abi.sig_unary,
                    Helper::ArrayAt,
                    &[this, arg1],
                )?,
                Intrinsic::ArrayIncludes => {
                    let from = args.get(1).copied().unwrap_or(undefined);
                    call_helper(
                        builder,
                        helpers,
                        abi,
                        abi.sig_binary,
                        Helper::ArrayIncludes,
                        &[this, arg1, from],
                    )?
                }
                _ => call_helper(
                    builder,
                    helpers,
                    abi,
                    abi.sig_unary,
                    Helper::ArrayPush,
                    &[this, arg1],
                )?,
            };
            // `ArrayIndexOf`'s sentinel is `undefined` (never a valid index);
            // the remaining kinds use the hole.
            let sentinel = if kind == Intrinsic::ArrayIndexOf {
                JsValue::Undefined.bits() as i64
            } else {
                JsValue::hole().bits() as i64
            };
            emit_intrinsic_sentinel(builder, result, sentinel, slow, merge, value);
        }
    }

    // The fallback: the general call (a shadowed method, or a receiver/argument
    // shape the helper declines). The args sit at the working-region base.
    builder.seal_block(slow);
    builder.switch_to_block(slow);
    for (k, a) in args.iter().enumerate() {
        builder.ins().store(
            MemFlagsData::new(),
            *a,
            abi.work,
            Offset32::new((k * 8) as i32),
        );
    }
    let argc_imm = builder.ins().iconst(types::I64, args.len() as i64);
    let dir_imm = builder.ins().iconst(types::I64, 0);
    let res = call_helper(
        builder,
        helpers,
        abi,
        abi.sig_call_slow,
        Helper::CallSlow,
        &[callee, this, argc_imm, abi.work, dir_imm],
    )?;
    builder.def_var(value, res);
    builder.ins().jump(merge, &[]);

    builder.seal_block(merge);
    builder.switch_to_block(merge);
    Ok(builder.use_var(value))
}

/// The sentinel tail of an intrinsic helper: when the result equals `sentinel`
/// (the helper declined the receiver), jump to `slow` (the general call);
/// otherwise store the result and jump to `merge`.
fn emit_intrinsic_sentinel(
    builder: &mut FunctionBuilder,
    result: ClifValue,
    sentinel: i64,
    slow: Block,
    merge: Block,
    value: Variable,
) {
    let sentinel = builder.ins().iconst(types::I64, sentinel);
    let fallback = builder.ins().icmp(IntCC::Equal, result, sentinel);
    let hit = builder.create_block();
    builder.ins().brif(fallback, slow, &[], hit, &[]);
    builder.seal_block(hit);
    builder.switch_to_block(hit);
    builder.def_var(value, result);
    builder.ins().jump(merge, &[]);
}

/// Emit a helper call with the pending-error ABI: with no try machinery a
/// helper that hit an interpreter error bails the body with `undefined` (the
/// runtime surfaces the stored error). Mirrors `call_slow`'s non-try path.
/// The validated member-name store (mirrors the per-step lowerer's
/// `emit_validated_member_store`): a plain-object receiver whose member-value
/// cell validates writes through the narrow `set_member_slot` (no generation
/// bump, no [[Set]] walk); every other receiver falls to the full
/// `set_member_name` helper, which is also the authoritative backstop when
/// `set_member_slot`'s own writable check fails.
fn emit_member_store(
    builder: &mut FunctionBuilder,
    helpers: &JitHelpers,
    abi: &Abi,
    object: ClifValue,
    value: ClifValue,
    name: crux::AtomId,
) -> Result<(), Unsupported> {
    let name_imm = builder.ins().iconst(types::I64, name as i64);
    let obj_ok = crate::cells::is_plain_object(builder, object);
    let validate = builder.create_block();
    let fast = builder.create_block();
    let slow = builder.create_block();
    let merge = builder.create_block();
    builder.ins().brif(obj_ok, validate, &[], slow, &[]);
    builder.switch_to_block(validate);
    let cells = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        abi.vm,
        Offset32::new(std::mem::offset_of!(JitCallContext, member_value_cells) as i32),
    );
    let ptr = crate::cells::object_data_ptr(builder, object);
    let live_id = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        ptr,
        Offset32::new(std::mem::offset_of!(crux::JsObject, id) as i32),
    );
    let live_gen = builder.ins().load(
        types::I32,
        MemFlagsData::new(),
        ptr,
        Offset32::new(std::mem::offset_of!(crux::JsObject, generation) as i32),
    );
    let cell = crate::cells::member_value_cell_addr(builder, cells, live_id, name_imm);
    let ok = crate::cells::member_value_cell_valid(builder, cell, live_id, name, live_gen);
    builder.ins().brif(ok, fast, &[], slow, &[]);
    builder.switch_to_block(fast);
    call_helper(
        builder,
        helpers,
        abi,
        abi.sig_binary,
        Helper::SetMemberSlot,
        &[object, name_imm, value],
    )?;
    builder.ins().jump(merge, &[]);
    builder.switch_to_block(slow);
    call_helper(
        builder,
        helpers,
        abi,
        abi.sig_binary,
        Helper::SetMemberName,
        &[object, name_imm, value],
    )?;
    builder.ins().jump(merge, &[]);
    builder.seal_block(merge);
    builder.switch_to_block(merge);
    Ok(())
}

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

/// Emit the shared tail of a speculation guard: branch on `ok`; on failure
/// mirror the live operand stack (`live`, bottom to top) into the working
/// region, point the context and `vm.ip` at the resume step, and return
/// `DISPATCH_DEOPT`; on success continue in a fresh block. The caller returns
/// its own result from the continuation block.
///
/// The `vm.ip` write is skipped when the ctx's `vm` pointer is null, matching
/// `emit_completion_store` — the scaffold's bare-ctx test harness runs a
/// guard's deopt with a null `vm`.
fn emit_guard(
    builder: &mut FunctionBuilder,
    abi: &Abi,
    ok: ClifValue,
    step: i32,
    live: &[ValueId],
    values: &[Option<ClifValue>],
) -> Result<(), Unsupported> {
    let deopt = builder.create_block();
    let cont = builder.create_block();
    builder.ins().brif(ok, cont, &[], deopt, &[]);
    builder.switch_to_block(deopt);
    for (i, v) in live.iter().enumerate() {
        let value = value_of(values, *v)?;
        builder.ins().store(
            MemFlagsData::new(),
            value,
            abi.work,
            Offset32::new((i * 8) as i32),
        );
    }
    let top = builder.ins().iadd_imm_u(abi.work, (live.len() * 8) as i64);
    builder.ins().store(
        MemFlagsData::new(),
        top,
        abi.vm,
        Offset32::new(std::mem::offset_of!(JitCallContext, suspend_sp) as i32),
    );
    let vm = builder.ins().load(
        types::I64,
        MemFlagsData::new(),
        abi.vm,
        Offset32::new(std::mem::offset_of!(JitCallContext, vm) as i32),
    );
    let has_vm = builder.ins().icmp_imm_u(IntCC::NotEqual, vm, 0);
    let store = builder.create_block();
    let ret = builder.create_block();
    builder.ins().brif(has_vm, store, &[], ret, &[]);
    builder.switch_to_block(store);
    let ip = builder.ins().iconst(types::I64, step as i64);
    builder.ins().store(
        MemFlagsData::new(),
        ip,
        vm,
        Offset32::new(VM_IP_OFFSET as i32),
    );
    builder.ins().jump(ret, &[]);
    builder.seal_block(store);
    builder.switch_to_block(ret);
    let sentinel = builder.ins().iconst(types::I64, DISPATCH_DEOPT as i64);
    builder.ins().return_(&[sentinel]);
    builder.seal_block(ret);
    builder.seal_block(deopt);
    builder.switch_to_block(cont);
    builder.seal_block(cont);
    Ok(())
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
    vals: (ClifValue, ClifValue, (bool, bool)),
) -> Result<ClifValue, Unsupported> {
    let (lhs, rhs, (lhs_known, rhs_known)) = vals;
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
    let lhs_dbl = if lhs_known {
        builder.ins().iconst(types::I8, 1)
    } else {
        is_double(builder, lhs)
    };
    let rhs_dbl = if rhs_known {
        builder.ins().iconst(types::I8, 1)
    } else {
        is_double(builder, rhs)
    };
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

/// The inline integer fast path for `%`: when both operands are Numbers,
/// integral, inside the i32 range and the divisor is nonzero — where the spec's
/// `fmod` equals the integer remainder — compute `srem`; otherwise fall through
/// to `BinarySlow`. Mirrors the per-step lowerer's `emit_mod_int` (Cranelift has
/// no `frem`, and `fmod` is a libm call while `srem` is an instruction).
fn call_mod_int(
    builder: &mut FunctionBuilder,
    helpers: &JitHelpers,
    abi: &Abi,
    disc: i64,
    lhs: ClifValue,
    rhs: ClifValue,
    operands_known: bool,
) -> Result<ClifValue, Unsupported> {
    let lhs_num = builder.ins().bitcast(types::F64, MemFlagsData::new(), lhs);
    let rhs_num = builder.ins().bitcast(types::F64, MemFlagsData::new(), rhs);
    let (l_wide, _) = trunc_i32(builder, lhs_num);
    let (r_wide, _) = trunc_i32(builder, rhs_num);
    let l_back = builder.ins().fcvt_from_sint(types::F64, l_wide);
    let r_back = builder.ins().fcvt_from_sint(types::F64, r_wide);
    // The truncation is only exact for integral values, so a fractional operand
    // takes the slow path.
    let l_int = builder.ins().fcmp(FloatCC::Equal, l_back, lhs_num);
    let r_int = builder.ins().fcmp(FloatCC::Equal, r_back, rhs_num);
    // `srem` on the i32 reduction is `a % b` only for |x| <= 2^31-1; a larger
    // integer would take the low 32 bits of the *rounded* value.
    let bound = builder.ins().f64const(2147483647.0);
    let l_abs = builder.ins().fabs(lhs_num);
    let r_abs = builder.ins().fabs(rhs_num);
    let l_in_i32 = builder.ins().fcmp(FloatCC::LessThanOrEqual, l_abs, bound);
    let r_in_i32 = builder.ins().fcmp(FloatCC::LessThanOrEqual, r_abs, bound);
    let l = builder.ins().ireduce(types::I32, l_wide);
    let r = builder.ins().ireduce(types::I32, r_wide);
    let r_nonzero = builder.ins().icmp_imm_u(IntCC::NotEqual, r, 0);
    let lhs_dbl = if operands_known {
        builder.ins().iconst(types::I8, 1)
    } else {
        is_double(builder, lhs)
    };
    let rhs_dbl = if operands_known {
        builder.ins().iconst(types::I8, 1)
    } else {
        is_double(builder, rhs)
    };
    let mut guard = builder.ins().band(lhs_dbl, rhs_dbl);
    guard = builder.ins().band(guard, l_int);
    guard = builder.ins().band(guard, r_int);
    guard = builder.ins().band(guard, l_in_i32);
    guard = builder.ins().band(guard, r_in_i32);
    guard = builder.ins().band(guard, r_nonzero);
    let result = builder.declare_var(types::I64);
    let merge = builder.create_block();
    let fast = builder.create_block();
    let slow = builder.create_block();
    builder.ins().brif(guard, fast, &[], slow, &[]);
    // The remainder runs only on the fast path: `srem` by zero traps, so the
    // nonzero guard has to gate the instruction itself.
    builder.switch_to_block(fast);
    let rem = builder.ins().srem(l, r);
    let wide = builder.ins().sextend(types::I64, rem);
    let rem_f = builder.ins().fcvt_from_sint(types::F64, wide);
    // `fmod` takes the dividend's sign; `srem` matches except for a zero result
    // (which it makes +0), so copy the dividend's sign onto it.
    let rem_f = builder.ins().fcopysign(rem_f, lhs_num);
    let bits = builder
        .ins()
        .bitcast(types::I64, MemFlagsData::new(), rem_f);
    let fast_res = canon_double(builder, bits);
    builder.def_var(result, fast_res);
    builder.ins().jump(merge, &[]);
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
    builder.seal_block(fast);
    builder.seal_block(slow);
    builder.seal_block(merge);
    builder.switch_to_block(merge);
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
