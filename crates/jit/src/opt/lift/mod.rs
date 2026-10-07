//! The lift: the interpreter's `Step` stream into the SSA IR.
//!
//! Increments I1 (straight line) and I2 (forward control flow): a `Jump`, a
//! `JumpIfFalse`/`JumpIfTrue` and a join. A step outside the subset, a body
//! that is not certified, a TDZ-checked slot, or a back edge (a loop, which
//! needs a fixpoint over the join stack — I2b) returns [`Unsupported`] and
//! keeps the current per-step lowering, so nothing about the engine's behavior
//! changes unless a later increment wires the IR in.
//!
//! The lift is exact by construction: an accepted step means exactly what the
//! interpreter's handler for that step means. Two shapes it deliberately
//! refuses rather than approximates:
//!
//! - **Loops.** A back edge's target needs parameters fed from a predecessor
//!   that has not been lifted yet; the forward-only shape resolves a join from
//!   already-lifted predecessors.
//! - **TDZ slots.** `Step::LoadLocal`/`StoreLocal` carry an
//!   `is_uninitialized` check the IR does not model (it will become an explicit
//!   op). A body whose scope has no TDZ slot — parameters and `var`s only — has
//!   no slot that can be uninitialized, so the check can never fire and the
//!   check-free `FrameLoad`/`FrameStore` are exact.
//!
//! Frame slots stay in memory (the IR's `FrameLoad`/`FrameStore` read and write
//! `Heap::Slots`); only the operand stack is SSA and needs join parameters.

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
    /// A step outside the subset (a back edge names `"loop"`).
    Step(&'static str),
    /// The body reached its end without a `Return`, so the entry block has no
    /// terminator.
    NoReturn,
    /// The operand stack underflowed — a step sequence the interpreter would
    /// not produce.
    Stack,
    /// A malformed body (a jump outside the body, mismatched stack depths at a
    /// join).
    Malformed(&'static str),
    /// The lift produced a graph [`verify`](super::verify::verify) rejects.
    /// This is a lift bug; the caller keeps the current path either way.
    Invalid,
}

/// Lift a certified body into the SSA IR (straight line + forward control
/// flow).
pub fn lift(body: &CompiledBody) -> Result<Function, Unsupported> {
    let scope = body.scope.as_ref().ok_or(Unsupported::Uncertified)?;
    if scope.tdz_store.iter().any(|&tdz| tdz) {
        return Err(Unsupported::TdzSlot);
    }
    let steps = &body.steps;
    let n = steps.len();
    if n == 0 {
        return Err(Unsupported::NoReturn);
    }

    // A block starts at 0, at every jump target, and after every terminator.
    let mut starts = vec![false; n + 1];
    starts[0] = true;
    for (i, step) in steps.iter().enumerate() {
        if let Some(t) = jump_target(step) {
            if t <= i {
                return Err(Unsupported::Step("loop"));
            }
            if t >= n {
                return Err(Unsupported::Malformed("jump out of range"));
            }
            starts[t] = true;
        }
        if is_terminator(step) {
            starts[i + 1] = true;
        }
    }
    let block_starts: Vec<usize> = (0..n).filter(|&i| starts[i]).collect();
    let nb = block_starts.len();
    let mut block_of = vec![usize::MAX; n];
    for (bi, &s) in block_starts.iter().enumerate() {
        block_of[s] = bi;
    }
    let ends: Vec<usize> = (0..nb)
        .map(|bi| block_starts.get(bi + 1).copied().unwrap_or(n))
        .collect();

    // Predecessors, from each block's successors (a jump target and, for a
    // conditional or a fall-through, the next block).
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); nb];
    for (bi, &end) in ends.iter().enumerate() {
        for succ in block_successors(steps, end, n)? {
            let sb = block_of[succ];
            if sb == usize::MAX {
                return Err(Unsupported::Malformed("jump to a non-block start"));
            }
            preds[sb].push(bi);
        }
    }

    let mut func = Function::new();
    for _ in 1..nb {
        func.push_block();
    }
    let mut builder = Builder::new(&mut func);
    let mut term_rec: Vec<Option<TermRec>> = (0..nb).map(|_| None).collect();

    // Emit instructions; the operand stack is SSA. A join block takes its stack
    // as parameters, and each predecessor passes its stack as edge arguments
    // (filled in below, once every block's parameters are known).
    for bi in 0..nb {
        let incoming: Vec<ValueId> = if bi == 0 {
            Vec::new()
        } else {
            let stacks: Vec<&[ValueId]> = preds[bi]
                .iter()
                .filter_map(|&p| term_rec[p].as_ref().map(edge_stack))
                .collect();
            let depth = stacks.first().map_or(0, |s| s.len());
            if stacks.iter().any(|s| s.len() != depth) {
                return Err(Unsupported::Malformed("stack depth mismatch at a join"));
            }
            if preds[bi].len() > 1 {
                (0..depth)
                    .map(|_| builder.param(bi as BlockId, Type::Unknown))
                    .collect()
            } else {
                stacks.first().map_or_else(Vec::new, |s| s.to_vec())
            }
        };
        let mut stack = incoming;
        let end = ends[bi];
        let term = terminator_index(steps, end);
        let body_end = term.unwrap_or(end);
        for step in &steps[block_starts[bi]..body_end] {
            emit_step(&mut builder, bi as BlockId, &mut stack, step)?;
        }
        term_rec[bi] = Some(match term {
            Some(ti) => match &steps[ti] {
                Step::Return => TermRec::Ret(stack.pop().ok_or(Unsupported::Stack)?),
                Step::Jump(_) => TermRec::Jump(stack),
                Step::JumpIfFalse(_) | Step::JumpIfTrue(_) => {
                    TermRec::Branch(stack.pop().ok_or(Unsupported::Stack)?, stack)
                }
                other => return Err(Unsupported::Step(step_name(other))),
            },
            None => TermRec::Jump(stack),
        });
    }

    // Terminators: every target's parameters are now known.
    for bi in 0..nb {
        let end = ends[bi];
        let term = terminator_index(steps, end);
        let rec = term_rec[bi].as_ref().ok_or(Unsupported::Invalid)?;
        match rec {
            TermRec::Ret(v) => builder.term(bi as BlockId, Term::Return(Some(*v))),
            TermRec::Jump(stack) => {
                let target = match term {
                    Some(ti) => jump_target(&steps[ti]).ok_or(Unsupported::Invalid)?,
                    None => end,
                };
                let sb = block_of[target] as BlockId;
                let args = edge_args(builder.func(), sb, stack);
                builder.term(bi as BlockId, Term::Jump { target: sb, args });
            }
            TermRec::Branch(cond, stack) => {
                let ti = term.ok_or(Unsupported::Invalid)?;
                let (target, truthy) = match &steps[ti] {
                    Step::JumpIfTrue(t) => (*t, true),
                    Step::JumpIfFalse(t) => (*t, false),
                    _ => return Err(Unsupported::Invalid),
                };
                let sb = block_of[target] as BlockId;
                let fb = block_of[end] as BlockId;
                let (then_block, else_block) = if truthy { (sb, fb) } else { (fb, sb) };
                let then_args = edge_args(builder.func(), then_block, stack);
                let else_args = edge_args(builder.func(), else_block, stack);
                builder.term(
                    bi as BlockId,
                    Term::Branch {
                        cond: *cond,
                        then_block,
                        then_args,
                        else_block,
                        else_args,
                    },
                );
            }
        }
    }

    if crate::opt::verify::verify(&func).is_err() {
        return Err(Unsupported::Invalid);
    }
    Ok(func)
}

/// The edge payload a block leaves on each successor.
enum TermRec {
    Ret(ValueId),
    Jump(Vec<ValueId>),
    Branch(ValueId, Vec<ValueId>),
}

fn edge_stack(rec: &TermRec) -> &[ValueId] {
    match rec {
        TermRec::Ret(_) => &[],
        TermRec::Jump(stack) | TermRec::Branch(_, stack) => stack,
    }
}

/// The arguments a predecessor passes into `target`: its stack, or nothing
/// when the target has a single predecessor and therefore no parameters.
fn edge_args(func: &Function, target: BlockId, stack: &[ValueId]) -> Vec<ValueId> {
    if func.block(target).params.is_empty() {
        Vec::new()
    } else {
        stack.to_vec()
    }
}

/// The index of the terminating step of a block ending at `end`, if it is a
/// terminator (`end - 1`); `None` for a fall-through block.
fn terminator_index(steps: &[Step], end: usize) -> Option<usize> {
    let last = end.checked_sub(1)?;
    is_terminator(&steps[last]).then_some(last)
}

/// The blocks a block ending at `end` can reach.
fn block_successors(steps: &[Step], end: usize, n: usize) -> Result<Vec<usize>, Unsupported> {
    let Some(last) = end.checked_sub(1) else {
        return Err(Unsupported::Invalid);
    };
    if is_terminator(&steps[last]) {
        match &steps[last] {
            Step::Jump(t) => Ok(vec![*t]),
            Step::JumpIfFalse(t) | Step::JumpIfTrue(t) => {
                if end >= n {
                    return Err(Unsupported::Malformed("conditional with no fall-through"));
                }
                Ok(vec![*t, end])
            }
            Step::Return => Ok(Vec::new()),
            Step::Throw { .. } => Err(Unsupported::Step("Throw")),
            _ => Err(Unsupported::Invalid),
        }
    } else if end >= n {
        Err(Unsupported::NoReturn)
    } else {
        Ok(vec![end])
    }
}

fn is_terminator(step: &Step) -> bool {
    matches!(
        step,
        Step::Jump(_)
            | Step::JumpIfFalse(_)
            | Step::JumpIfTrue(_)
            | Step::Return
            | Step::Throw { .. }
    )
}

fn jump_target(step: &Step) -> Option<usize> {
    match step {
        Step::Jump(t) | Step::JumpIfFalse(t) | Step::JumpIfTrue(t) => Some(*t),
        _ => None,
    }
}

fn emit_step(
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
        Step::Jump(_) | Step::JumpIfFalse(_) | Step::JumpIfTrue(_) => "control",
        Step::Return => "Return",
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

    fn ops(func: &Function, block: BlockId) -> Vec<Op> {
        func.block(block).insts.iter().map(|i| i.op).collect()
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
            ops(&func, entry),
            vec![Op::Const, Op::Const, Op::Add, Op::FrameLoad, Op::Sub]
        );
        let add = func.block(entry).insts[2].result.unwrap();
        let load = func.block(entry).insts[3].result.unwrap();
        assert_eq!(func.block(entry).insts[4].args, vec![add, load]);
        assert!(matches!(
            func.block(entry).term,
            Some(Term::Return(Some(_)))
        ));
    }

    #[test]
    fn lifts_a_ternary_branch() {
        // return f[0] ? 1 : 2;
        //  0 LoadLocal 0
        //  1 JumpIfFalse 4
        //  2 Push 1
        //  3 Jump 5
        //  4 Push 2
        //  5 Return
        let b = body(
            vec![
                Step::LoadLocal { slot: 0 },
                Step::JumpIfFalse(4),
                Step::Push(Value::Number(1.0)),
                Step::Jump(5),
                Step::Push(Value::Number(2.0)),
                Step::Return,
            ],
            1,
        );
        let func = lift(&b).expect("lifts");
        // Blocks: [0,2) branch, [2,4) then, [4,5) else, [5,6) join.
        assert_eq!(func.block_count(), 4);
        assert!(matches!(func.block(0).term, Some(Term::Branch { .. })));
        // The join block takes the two branch values as its parameter.
        assert_eq!(func.block(3).params.len(), 1);
        assert!(matches!(func.block(3).term, Some(Term::Return(Some(_)))));
        assert!(ops(&func, 2).contains(&Op::Const));
        assert!(ops(&func, 1).contains(&Op::Const));
    }

    #[test]
    fn lifts_push_pop_dup_and_binary_imm() {
        // A dead push/pop pair and a dup-ed multiply, then `f[1] + 4`.
        let b = body(
            vec![
                Step::Push(Value::Number(7.0)),
                Step::Pop,
                Step::Push(Value::Number(3.0)),
                Step::Dup,
                Step::Binary(BinaryOp::Mul),
                Step::LoadLocal { slot: 1 },
                Step::BinaryImm {
                    op: BinaryOp::Add,
                    imm: 4.0,
                },
                Step::Return,
            ],
            2,
        );
        let func = lift(&b).expect("lifts");
        let entry = func.entry();
        assert_eq!(func.block(entry).insts[4].imm, Imm::Float(4.0));
    }

    #[test]
    fn lifts_stores_and_unary() {
        // var x = f[0]; x = -x; return x;
        let b = body(
            vec![
                Step::LoadLocal { slot: 0 },
                Step::InitLocal { slot: 1 },
                Step::LoadLocal { slot: 1 },
                Step::Unary(UnaryOp::Minus),
                Step::StoreLocal { slot: 1 },
                Step::LoadLocal { slot: 1 },
                Step::Return,
            ],
            2,
        );
        let func = lift(&b).expect("lifts");
        let entry = func.entry();
        assert_eq!(
            ops(&func, entry),
            vec![
                Op::FrameLoad,
                Op::FrameStore,
                Op::FrameLoad,
                Op::Neg,
                Op::FrameStore,
                Op::FrameLoad,
            ]
        );
        assert_eq!(func.block(entry).insts[1].imm, Imm::Slot(1));
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
    fn a_back_edge_is_unsupported() {
        let b = body(vec![Step::Jump(0)], 0);
        assert_eq!(lift(&b).unwrap_err(), Unsupported::Step("loop"));
    }

    #[test]
    fn a_throw_is_unsupported() {
        let b = body(
            vec![
                Step::Push(Value::Number(1.0)),
                Step::Throw {
                    span: crux::Span::empty(0),
                },
            ],
            0,
        );
        assert_eq!(lift(&b).unwrap_err(), Unsupported::Step("Throw"));
    }

    #[test]
    fn a_non_numeric_constant_is_unsupported() {
        // A bare `return;` pushes `undefined`, which the constant set omits.
        let b = body(vec![Step::Push(Value::Undefined), Step::Return], 0);
        assert_eq!(lift(&b).unwrap_err(), Unsupported::Step("Push"));
    }

    #[test]
    fn a_body_without_a_return_is_unsupported() {
        let b = body(vec![Step::Push(Value::Number(1.0))], 0);
        assert_eq!(lift(&b).unwrap_err(), Unsupported::NoReturn);
    }

    #[test]
    fn constant_folds_nothing_but_types_the_result() {
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
}
