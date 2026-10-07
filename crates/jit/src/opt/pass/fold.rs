//! Constant folding over the SSA IR (`.notes/optimizing-tier-impl.md` §2,
//! `pass/fold.rs`).
//!
//! Folding is exact: it rewrites an op only when its operands are constants of a
//! kind that makes the op's JS semantics a plain IEEE-754 operation — two
//! numbers for `+`/`-`/`*`/`/` and the ordering comparisons, two numbers or two
//! booleans for equality, a boolean for `!`, a number for unary `-`. Everything
//! else is left alone: a coercing `+` (a string operand), the bitwise ops (which
//! need `ToInt32`), and `%`/`**` (whose edge cases the typer's narrower folds can
//! own). An operand is only folded when its constant is defined in the same
//! block (so it dominates the use by the verifier's rule); a constant defined in
//! a dominating block is left as a missed fold, never a wrong one.

use crate::opt::ir::{Block, Effects, Function, Imm, Inst, Op, Type, ValueId};

/// How many folds the pipeline has performed (test introspection).
#[cfg(test)]
pub(crate) static FOLDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Fold every foldable instruction in `func`, repeating until no fold remains
/// (a fold can expose the next, e.g. `(1 + 2) * 3`). Returns whether anything
/// changed. Termination holds: each fold replaces a non-`Const` op with a
/// `Const`, so the count of foldable ops strictly decreases.
pub fn run(func: &mut Function) -> bool {
    let mut changed = false;
    loop {
        if !pass(func) {
            return changed;
        }
        changed = true;
    }
}

/// One folding sweep.
fn pass(func: &mut Function) -> bool {
    let mut changed = false;
    for b in 0..func.block_count() as u32 {
        // Collect the fold sites first, so the read borrow of the block ends
        // before the write.
        let folds: Vec<(usize, Imm, Type)> = {
            let block = func.block(b);
            block
                .insts
                .iter()
                .enumerate()
                .filter_map(|(i, inst)| fold_inst(block, inst).map(|(imm, ty)| (i, imm, ty)))
                .collect()
        };
        if folds.is_empty() {
            continue;
        }
        let block = func.block_mut(b);
        for (i, imm, ty) in folds {
            let inst = &mut block.insts[i];
            inst.op = Op::Const;
            inst.args.clear();
            inst.ty = ty;
            inst.effects = Effects::pure();
            inst.imm = imm;
            #[cfg(test)]
            FOLDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            changed = true;
        }
    }
    changed
}

/// A foldable constant operand.
#[derive(Clone, Copy)]
enum ConstVal {
    Num(f64),
    Bool(bool),
}

/// The constant an instruction computes, or `None` when it is not foldable.
fn fold_inst(block: &Block, inst: &Inst) -> Option<(Imm, Type)> {
    // A `Const` is already folded; an op with no result is left alone.
    if inst.op == Op::Const || inst.result.is_none() {
        return None;
    }
    let mut operands = inst.args.iter().map(|&a| const_value(block, a));
    match inst.op {
        Op::Add | Op::Sub | Op::Mul | Op::Div => {
            let (ConstVal::Num(a), ConstVal::Num(b)) = (operands.next()??, operands.next()??)
            else {
                return None;
            };
            let result = match inst.op {
                Op::Add => a + b,
                Op::Sub => a - b,
                Op::Mul => a * b,
                _ => a / b,
            };
            Some((Imm::Float(result), Type::Number))
        }
        Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            let (ConstVal::Num(a), ConstVal::Num(b)) = (operands.next()??, operands.next()??)
            else {
                return None;
            };
            let result = match inst.op {
                Op::Lt => a < b,
                Op::Le => a <= b,
                Op::Gt => a > b,
                _ => a >= b,
            };
            Some((Imm::Bool(result), Type::Bool))
        }
        // Same-kind equality is strict equality (a primitive never coerces to
        // the other primitive kind under `==`).
        Op::Eq | Op::StrictEq => {
            let result = match (operands.next()??, operands.next()??) {
                (ConstVal::Num(a), ConstVal::Num(b)) => a == b,
                (ConstVal::Bool(a), ConstVal::Bool(b)) => a == b,
                _ => return None,
            };
            Some((Imm::Bool(result), Type::Bool))
        }
        Op::Neg => {
            let ConstVal::Num(a) = operands.next()?? else {
                return None;
            };
            Some((Imm::Float(-a), Type::Number))
        }
        Op::Not => {
            let ConstVal::Bool(a) = operands.next()?? else {
                return None;
            };
            Some((Imm::Bool(!a), Type::Bool))
        }
        _ => None,
    }
}

/// The constant a value is defined as, when it is a `Const` in `block`.
fn const_value(block: &Block, v: ValueId) -> Option<ConstVal> {
    block
        .insts
        .iter()
        .find(|inst| inst.result == Some(v))
        .and_then(|inst| match (&inst.op, &inst.imm) {
            (Op::Const, Imm::Float(f)) => Some(ConstVal::Num(*f)),
            (Op::Const, Imm::Int(i)) => Some(ConstVal::Num(f64::from(*i))),
            (Op::Const, Imm::Bool(b)) => Some(ConstVal::Bool(*b)),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::Term;
    use crate::opt::pass::dce;

    fn num_const(b: &mut Builder<'_>, block: u32, v: f64) -> ValueId {
        b.emit(
            block,
            Op::Const,
            &[],
            Type::Number,
            Effects::pure(),
            Imm::Float(v),
        )
    }

    fn binary(b: &mut Builder<'_>, block: u32, op: Op, lhs: ValueId, rhs: ValueId) -> ValueId {
        b.emit(
            block,
            op,
            &[lhs, rhs],
            Type::Number,
            Effects::call(),
            Imm::None,
        )
    }

    fn const_of(func: &Function, block: u32, v: ValueId) -> Option<Imm> {
        func.block(block)
            .insts
            .iter()
            .find(|i| i.result == Some(v))
            .map(|i| i.imm.clone())
    }

    #[test]
    fn folds_numeric_arithmetic() {
        let mut func = Function::new();
        let entry = func.entry();
        let sum;
        {
            let mut b = Builder::new(&mut func);
            let one = num_const(&mut b, entry, 1.0);
            let two = num_const(&mut b, entry, 2.0);
            sum = binary(&mut b, entry, Op::Add, one, two);
            b.term(entry, Term::Return(Some(sum)));
        }
        assert!(run(&mut func));
        assert_eq!(const_of(&func, entry, sum), Some(Imm::Float(3.0)));
    }

    #[test]
    fn folds_equality_and_not() {
        let mut func = Function::new();
        let entry = func.entry();
        let (eq, not);
        {
            let mut b = Builder::new(&mut func);
            let a = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Bool,
                Effects::pure(),
                Imm::Bool(true),
            );
            let c = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Bool,
                Effects::pure(),
                Imm::Bool(true),
            );
            eq = b.emit(
                entry,
                Op::StrictEq,
                &[a, c],
                Type::Bool,
                Effects::pure(),
                Imm::None,
            );
            not = b.emit(
                entry,
                Op::Not,
                &[eq],
                Type::Bool,
                Effects::pure(),
                Imm::None,
            );
            b.term(entry, Term::Return(Some(eq)));
        }
        assert!(run(&mut func));
        assert_eq!(const_of(&func, entry, eq), Some(Imm::Bool(true)));
        assert_eq!(const_of(&func, entry, not), Some(Imm::Bool(false)));
    }

    #[test]
    fn does_not_fold_a_mixed_kind_equality() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let n = num_const(&mut b, entry, 1.0);
            let t = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Bool,
                Effects::pure(),
                Imm::Bool(true),
            );
            // `1 == true` is `true`, but the fold must decline: a mixed kind
            // needs the coercing definition this pass does not own.
            let eq = b.emit(
                entry,
                Op::Eq,
                &[n, t],
                Type::Bool,
                Effects::call(),
                Imm::None,
            );
            b.term(entry, Term::Return(Some(eq)));
        }
        assert!(!run(&mut func));
        assert_eq!(func.block(entry).insts.last().unwrap().op, Op::Eq);
    }

    #[test]
    fn a_fold_lets_dce_drop_the_dead_constants() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let one = num_const(&mut b, entry, 1.0);
            let two = num_const(&mut b, entry, 2.0);
            let sum = binary(&mut b, entry, Op::Add, one, two);
            b.term(entry, Term::Return(Some(sum)));
        }
        assert!(run(&mut func));
        assert!(dce::run(&mut func));
        // The two operand constants are gone; only the folded `Const` remains.
        let insts = &func.block(entry).insts;
        assert_eq!(insts.len(), 1);
        assert_eq!(insts[0].op, Op::Const);
        assert_eq!(insts[0].imm, Imm::Float(3.0));
    }

    #[test]
    fn a_string_operand_is_not_folded() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let s = b.emit(
                entry,
                Op::Const,
                &[],
                Type::String,
                Effects::pure(),
                Imm::Str("x".into()),
            );
            let one = num_const(&mut b, entry, 1.0);
            let add = binary(&mut b, entry, Op::Add, s, one);
            b.term(entry, Term::Return(Some(add)));
        }
        assert!(!run(&mut func));
    }
}
