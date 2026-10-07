//! The lift: the interpreter's `Step` stream into the SSA IR.
//!
//! Increment I1 of `.notes/optimizing-tier-impl.md` §9: the straight-line
//! subset. A step outside the subset, or a body that is not certified or has a
//! TDZ-checked slot, returns [`Unsupported`] and keeps the current per-step
//! lowering, so nothing about the engine's behavior changes until a later
//! increment wires the IR in.
//!
//! The lift is exact by construction: an accepted step means exactly what the
//! interpreter's handler for that step means. Two shapes it deliberately
//! refuses rather than approximates:
//!
//! - **Control flow.** A `Jump`/`JumpIf*` needs block boundaries and join phis
//!   (I2); the straight-line form has a single block.
//! - **TDZ slots.** `Step::LoadLocal`/`StoreLocal` carry an
//!   `is_uninitialized` check the IR does not model (it will become an explicit
//!   op). A body whose scope has no TDZ slot — parameters and `var`s only — has
//!   no slot that can be uninitialized, so the check can never fire and the
//!   check-free `FrameLoad`/`FrameStore` are exact.

use crate::opt::builder::Builder;
use crate::opt::ir::{BlockId, Effects, Function, Heap, Imm, Op, Term, Type, ValueId};
use syntax::ast::{BinaryOp, UnaryOp};

use runtime::ir::{CompiledBody, Step};

/// Why a body could not be lifted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unsupported {
    /// The body is not certified (`CompiledBody::scope` is `None`): it has no
    /// frame slots to lift against.
    Uncertified,
    /// The scope has a TDZ-checked slot (see the module note).
    TdzSlot,
    /// A step outside the straight-line subset.
    Step(&'static str),
    /// The body reached its end without a `Return`, so the entry block has no
    /// terminator.
    NoReturn,
    /// The operand stack underflowed — a step sequence the interpreter would
    /// not produce.
    Stack,
    /// The lift produced a graph [`verify`](super::verify::verify) rejects.
    /// This is a lift bug; the caller keeps the current path either way.
    Invalid,
}

/// Lift a certified straight-line body into the SSA IR.
pub fn lift(body: &CompiledBody) -> Result<Function, Unsupported> {
    let scope = body.scope.as_ref().ok_or(Unsupported::Uncertified)?;
    if scope.tdz_store.iter().any(|&tdz| tdz) {
        return Err(Unsupported::TdzSlot);
    }

    let mut func = Function::new();
    let entry = func.entry();
    {
        let mut builder = Builder::new(&mut func);
        let mut stack: Vec<ValueId> = Vec::new();
        for step in &body.steps {
            lift_step(&mut builder, entry, &mut stack, step)?;
        }
    }

    if !matches!(func.block(entry).term, Some(Term::Return(_))) {
        return Err(Unsupported::NoReturn);
    }
    if crate::opt::verify::verify(&func).is_err() {
        return Err(Unsupported::Invalid);
    }
    Ok(func)
}

fn lift_step(
    builder: &mut Builder,
    block: BlockId,
    stack: &mut Vec<ValueId>,
    step: &Step,
) -> Result<(), Unsupported> {
    match step {
        Step::Push(value) => {
            let (imm, ty) = constant(value)?;
            let v = builder.emit(block, Op::Const, &[], ty, Effects::pure(), imm);
            stack.push(v);
        }
        Step::Pop => {
            stack.pop().ok_or(Unsupported::Stack)?;
        }
        Step::Dup => {
            let top = *stack.last().ok_or(Unsupported::Stack)?;
            stack.push(top);
        }
        Step::LoadLocal { slot } => {
            let v = builder.emit(
                block,
                Op::FrameLoad,
                &[],
                Type::Unknown,
                Effects::read(Heap::Slots),
                Imm::Slot(*slot as u32),
            );
            stack.push(v);
        }
        Step::StoreLocal { slot } | Step::InitLocal { slot } => {
            let value = stack.pop().ok_or(Unsupported::Stack)?;
            builder.emit_void(
                block,
                Op::FrameStore,
                &[value],
                Effects::write(Heap::Slots),
                Imm::Slot(*slot as u32),
            );
        }
        Step::Binary(op) => {
            let right = stack.pop().ok_or(Unsupported::Stack)?;
            let left = stack.pop().ok_or(Unsupported::Stack)?;
            emit_binary(builder, block, stack, *op, left, right)?;
        }
        Step::BinaryImm { op, imm } => {
            let left = stack.pop().ok_or(Unsupported::Stack)?;
            let right = builder.emit(
                block,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(*imm),
            );
            emit_binary(builder, block, stack, *op, left, right)?;
        }
        Step::Unary(op) => {
            let operand = stack.pop().ok_or(Unsupported::Stack)?;
            let op = unary_op(*op)?;
            let v = builder.emit(
                block,
                op,
                &[operand],
                unary_type(op),
                op.default_effects(),
                Imm::None,
            );
            stack.push(v);
        }
        Step::Return => {
            let value = stack.pop().ok_or(Unsupported::Stack)?;
            builder.term(block, Term::Return(Some(value)));
        }
        other => return Err(Unsupported::Step(step_name(other))),
    }
    Ok(())
}

fn emit_binary(
    builder: &mut Builder,
    block: BlockId,
    stack: &mut Vec<ValueId>,
    op: BinaryOp,
    left: ValueId,
    right: ValueId,
) -> Result<(), Unsupported> {
    let op = binary_op(op)?;
    let v = builder.emit(
        block,
        op,
        &[left, right],
        binary_type(op),
        op.default_effects(),
        Imm::None,
    );
    stack.push(v);
    Ok(())
}

fn constant(value: &crux::Value) -> Result<(Imm, Type), Unsupported> {
    if let Some(n) = value.as_number() {
        return Ok((Imm::Float(n), Type::Number));
    }
    if let Some(b) = value.as_boolean() {
        return Ok((Imm::Bool(b), Type::Bool));
    }
    Err(Unsupported::Step("Push"))
}

fn binary_op(op: BinaryOp) -> Result<Op, Unsupported> {
    Ok(match op {
        BinaryOp::Exp => Op::Pow,
        BinaryOp::Mul => Op::Mul,
        BinaryOp::Div => Op::Div,
        BinaryOp::Rem => Op::Mod,
        BinaryOp::Add => Op::Add,
        BinaryOp::Sub => Op::Sub,
        BinaryOp::LeftShift => Op::Shl,
        BinaryOp::RightShift => Op::Shr,
        BinaryOp::UnsignedRightShift => Op::UShr,
        BinaryOp::LessThan => Op::Lt,
        BinaryOp::GreaterThan => Op::Gt,
        BinaryOp::LessEqual => Op::Le,
        BinaryOp::GreaterEqual => Op::Ge,
        BinaryOp::Equal => Op::Eq,
        BinaryOp::StrictEqual => Op::StrictEq,
        BinaryOp::BitAnd => Op::BitAnd,
        BinaryOp::BitXor => Op::BitXor,
        BinaryOp::BitOr => Op::BitOr,
        BinaryOp::In | BinaryOp::Instanceof | BinaryOp::NotEqual | BinaryOp::StrictNotEqual => {
            return Err(Unsupported::Step("Binary"));
        }
    })
}

fn unary_op(op: UnaryOp) -> Result<Op, Unsupported> {
    Ok(match op {
        UnaryOp::Plus => Op::ToNumber,
        UnaryOp::Minus => Op::Neg,
        UnaryOp::BitNot => Op::BitNot,
        UnaryOp::Not => Op::Not,
        UnaryOp::Delete | UnaryOp::Void | UnaryOp::Typeof => {
            return Err(Unsupported::Step("Unary"));
        }
    })
}

fn binary_type(op: Op) -> Type {
    match op {
        Op::Eq | Op::StrictEq | Op::Lt | Op::Le | Op::Gt | Op::Ge => Type::Bool,
        Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr => Type::Int,
        _ => Type::Unknown,
    }
}

fn unary_type(op: Op) -> Type {
    match op {
        Op::Not => Type::Bool,
        Op::BitNot => Type::Int,
        _ => Type::Number,
    }
}

fn step_name(step: &Step) -> &'static str {
    match step {
        Step::Jump(_) => "Jump",
        Step::JumpIfFalse(_) => "JumpIfFalse",
        Step::JumpIfTrue(_) => "JumpIfTrue",
        _ => "step",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::ir::{Imm, Op, Term, Type};
    use crux::Value;
    use runtime::ir::{CompiledBody, ScopeInfo};

    /// A certified-style scope for the hand-built test bodies: `frame_size`
    /// slots, all `var`-like (no TDZ), nothing captured.
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

    fn body(steps: Vec<Step>, frame_size: usize) -> CompiledBody {
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
        }
    }

    fn ops(func: &Function) -> Vec<Op> {
        func.block(func.entry())
            .insts
            .iter()
            .map(|i| i.op)
            .collect()
    }

    #[test]
    fn lifts_a_straight_line_arithmetic_body() {
        // return (1 + 2) - f[0];
        let b = body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Push(Value::Number(2.0)),
                Step::Binary(BinaryOp::Add),
                Step::LoadLocal { slot: 0 },
                Step::Binary(BinaryOp::Sub),
                Step::Return,
            ],
            1,
        );
        let func = lift(&b).expect("lifts");
        assert_eq!(func.block_count(), 1);
        let entry = func.entry();
        assert_eq!(
            ops(&func),
            vec![Op::Const, Op::Const, Op::Add, Op::FrameLoad, Op::Sub]
        );
        // The `Add` result feeds the `Sub`, and the `FrameLoad` is its rhs.
        let add = func.block(entry).insts[2].result.unwrap();
        let load = func.block(entry).insts[3].result.unwrap();
        assert_eq!(func.block(entry).insts[4].args, vec![add, load]);
        assert!(matches!(
            func.block(entry).term,
            Some(Term::Return(Some(_)))
        ));
    }

    #[test]
    fn constant_folds_nothing_but_types_the_result() {
        // The lift types a comparison Bool and a bit op Int without folding.
        let b = body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Push(Value::Number(2.0)),
                Step::Binary(BinaryOp::StrictEqual),
                Step::Push(Value::Number(3.0)),
                Step::Binary(BinaryOp::BitOr),
                Step::Return,
            ],
            0,
        );
        let func = lift(&b).expect("lifts");
        let entry = func.entry();
        assert_eq!(func.block(entry).insts[2].ty, Type::Bool);
        assert_eq!(func.block(entry).insts[4].ty, Type::Int);
    }

    #[test]
    fn lifts_dup_and_pop() {
        // A dead push/pop pair and a dup-ed multiply: return 3 * 3.
        let b = body(
            vec![
                Step::Push(Value::Number(7.0)),
                Step::Pop,
                Step::Push(Value::Number(3.0)),
                Step::Dup,
                Step::Binary(BinaryOp::Mul),
                Step::Return,
            ],
            0,
        );
        let func = lift(&b).expect("lifts");
        assert_eq!(ops(&func), vec![Op::Const, Op::Const, Op::Mul]);
        // The multiply reads the same value twice.
        let mul = &func.block(func.entry()).insts[2];
        assert_eq!(mul.args[0], mul.args[1]);
    }

    #[test]
    fn lifts_binary_imm_and_stores() {
        // var x = f[0]; x = x + 4; return x;
        let b = body(
            vec![
                Step::LoadLocal { slot: 0 },
                Step::InitLocal { slot: 1 },
                Step::LoadLocal { slot: 1 },
                Step::BinaryImm {
                    op: BinaryOp::Add,
                    imm: 4.0,
                },
                Step::StoreLocal { slot: 1 },
                Step::LoadLocal { slot: 1 },
                Step::Return,
            ],
            2,
        );
        let func = lift(&b).expect("lifts");
        let entry = func.entry();
        assert_eq!(
            ops(&func),
            vec![
                Op::FrameLoad,
                Op::FrameStore,
                Op::FrameLoad,
                Op::Const,
                Op::Add,
                Op::FrameStore,
                Op::FrameLoad,
            ]
        );
        assert_eq!(func.block(entry).insts[3].imm, Imm::Float(4.0));
        // The stores name their slots.
        assert_eq!(func.block(entry).insts[1].imm, Imm::Slot(1));
        assert_eq!(func.block(entry).insts[5].imm, Imm::Slot(1));
    }

    #[test]
    fn lifts_unary() {
        // return -(f[0]);
        let b = body(
            vec![
                Step::LoadLocal { slot: 0 },
                Step::Unary(UnaryOp::Minus),
                Step::Return,
            ],
            1,
        );
        let func = lift(&b).expect("lifts");
        assert_eq!(ops(&func), vec![Op::FrameLoad, Op::Neg]);
        assert_eq!(func.block(func.entry()).insts[1].ty, Type::Number);
    }

    #[test]
    fn an_uncertified_body_is_unsupported() {
        let mut b = body(vec![Step::Push(Value::Number(1.0)), Step::Return], 0);
        b.scope = None;
        assert_eq!(lift(&b).unwrap_err(), Unsupported::Uncertified);
    }

    #[test]
    fn a_tdz_slot_is_unsupported() {
        let mut b = body(vec![Step::Push(Value::Number(1.0)), Step::Return], 1);
        b.scope = Some(ScopeInfo {
            tdz_store: vec![true],
            ..scope(1)
        });
        assert_eq!(lift(&b).unwrap_err(), Unsupported::TdzSlot);
    }

    #[test]
    fn a_jump_is_unsupported() {
        let b = body(vec![Step::Jump(0)], 0);
        assert_eq!(lift(&b).unwrap_err(), Unsupported::Step("Jump"));
    }

    #[test]
    fn a_non_numeric_constant_is_unsupported() {
        // A bare `return;` pushes `undefined`, which the I1 constant set omits.
        let b = body(vec![Step::Push(Value::Undefined), Step::Return], 0);
        assert_eq!(lift(&b).unwrap_err(), Unsupported::Step("Push"));
    }

    #[test]
    fn a_body_without_a_return_is_unsupported() {
        let b = body(vec![Step::Push(Value::Number(1.0))], 0);
        assert_eq!(lift(&b).unwrap_err(), Unsupported::NoReturn);
    }
}
