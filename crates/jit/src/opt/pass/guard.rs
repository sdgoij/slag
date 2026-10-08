//! Prune the typer's read guards that pay nothing.
//!
//! The lift emits `Op::GuardType` on a member/element read's result for every
//! read (`.notes/tier-typer.md` T2). A guard only pays where a *type-sensitive*
//! op consumes the value — the ops whose lowering reads the operand type to pick
//! the cheaper path (`emit_bare_numeric`, or the known-operand tag-check skip).
//! Everywhere else (the read flows to a call argument, a `return`, a member
//! store) the guard is a check with no payoff, and a wrong-type read deopts for
//! nothing — measured as a ~1% corpus-wide cost that offsets the read-loop win.
//!
//! A `FrameStore` keeps its guard: `narrow` reads the *stored value's* type to
//! decide a slot is numeric, so a guarded store is what makes a later arithmetic
//! read of that slot tag-free. An edge argument keeps it too — the value may
//! cross a block boundary and be consumed type-sensitively there.
//!
//! Dropping a guard is pure type erasure: the guard's result *is* its operand's
//! runtime value, so every use forwards to `args[0]`. Sound because the guard
//! only removes a type annotation, and a pruned guard is one no type-sensitive
//! op reads that annotation from.

use crate::opt::ir::{Function, Op, Term, ValueId};

/// Whether an op reads its operand's *type* to choose a cheaper lowering.
///
/// The bitwise/shift ops do not (their lowering is type-blind), nor `Eq`/`!`
/// (which take the slow helper regardless), nor the coercing unaries.
fn type_sensitive(op: Op) -> bool {
    matches!(
        op,
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Lt | Op::Le | Op::Gt | Op::Ge
    )
}

/// Drop every read guard no type-sensitive op consumes. Returns whether the IR
/// changed.
pub fn run(func: &mut Function) -> bool {
    let n = func.value_count() as usize;
    // A value is "kept" when some use reads its type: a type-sensitive op, a
    // slot store (narrow's numeric-slot proof feeds on it), or an edge argument
    // (the value may be consumed type-sensitively in the target block).
    let mut keep = vec![false; n];
    for b in 0..func.block_count() as u32 {
        let block = func.block(b);
        for inst in &block.insts {
            if type_sensitive(inst.op) || inst.op == Op::FrameStore {
                for &a in &inst.args {
                    keep[a as usize] = true;
                }
            }
        }
        match block.term.as_ref() {
            Some(Term::Jump { args, .. }) => {
                for &a in args {
                    keep[a as usize] = true;
                }
            }
            Some(Term::Branch {
                then_args,
                else_args,
                ..
            }) => {
                for &a in then_args.iter().chain(else_args) {
                    keep[a as usize] = true;
                }
            }
            _ => {}
        }
    }

    let mut subst: Vec<Option<ValueId>> = vec![None; n];
    let mut changed = false;
    for b in 0..func.block_count() as u32 {
        func.block_mut(b).insts.retain(|inst| {
            if inst.op != Op::GuardType {
                return true;
            }
            let Some(r) = inst.result else { return true };
            if keep[r as usize] {
                return true;
            }
            let Some(&operand) = inst.args.first() else {
                return true;
            };
            subst[r as usize] = Some(operand);
            changed = true;
            false
        });
    }
    if !changed {
        return false;
    }

    let resolve = |subst: &[Option<ValueId>], mut v: ValueId| {
        while let Some(next) = subst[v as usize] {
            v = next;
        }
        v
    };
    for b in 0..func.block_count() as u32 {
        let block = func.block_mut(b);
        for inst in &mut block.insts {
            for a in &mut inst.args {
                *a = resolve(&subst, *a);
            }
        }
        if let Some(term) = &mut block.term {
            match term {
                Term::Jump { args, .. } => {
                    for a in args {
                        *a = resolve(&subst, *a);
                    }
                }
                Term::Branch {
                    cond,
                    then_args,
                    else_args,
                    ..
                } => {
                    *cond = resolve(&subst, *cond);
                    for a in then_args.iter_mut().chain(else_args) {
                        *a = resolve(&subst, *a);
                    }
                }
                Term::Return(Some(v)) | Term::Throw(v) => *v = resolve(&subst, *v),
                _ => {}
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::{Effects, Imm, Type};

    /// `guard = GuardType(read); consumer(guard)` with a chosen consumer.
    fn body(consumer: Op, consumer_ty: Type) -> Function {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let obj = b.emit(
                entry,
                Op::FrameLoad,
                &[],
                Type::Object,
                Effects::read(crate::opt::ir::Heap::Slots),
                Imm::Slot(0),
            );
            let read = b.emit(
                entry,
                Op::MemberGuard,
                &[obj, obj],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            let guarded = b.emit(
                entry,
                Op::GuardType,
                &[read, read],
                Type::Number,
                Effects::pure(),
                Imm::Int(1),
            );
            let out = b.emit(
                entry,
                consumer,
                &[guarded],
                consumer_ty,
                Effects::call(),
                Imm::None,
            );
            b.term(entry, Term::Return(Some(out)));
        }
        func
    }

    fn guards(func: &Function) -> usize {
        func.block(func.entry())
            .insts
            .iter()
            .filter(|i| i.op == Op::GuardType)
            .count()
    }

    #[test]
    fn keeps_a_guard_an_arithmetic_op_consumes() {
        let mut func = body(Op::Add, Type::Number);
        assert!(!run(&mut func), "an Add consumes the guard's type");
        assert_eq!(guards(&func), 1);
    }

    #[test]
    fn drops_a_guard_a_call_consumes() {
        // A read passed as a call argument: the guard's type is never read, so
        // it is pure overhead (and would deopt for nothing on a wrong type).
        let mut func = body(Op::Call, Type::Number);
        assert!(run(&mut func));
        assert_eq!(guards(&func), 0);
        // The call now reads the read's value directly.
        let insts = &func.block(func.entry()).insts;
        let call = insts.iter().find(|i| i.op == Op::Call).expect("call");
        assert_eq!(call.args[0], insts[1].result.expect("read result"));
    }
}
