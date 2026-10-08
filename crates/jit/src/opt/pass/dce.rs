//! Dead-code elimination over the SSA IR (`.notes/optimizing-tier-impl.md` §2,
//! `pass/dce.rs`).
//!
//! An instruction is dead when its result is used by nothing and it is safe to
//! drop. Removal is by result-use, so it never touches an effect-only
//! instruction (a store, a completion write) — those have no result and are
//! kept. Eligible: a `pure` instruction (no reads, no writes) and an
//! `Op::FrameLoad`, a private-slot read that cannot trap. `Op::Check` and
//! `Op::GuardType` are deliberately exempt: their `default_effects` are pure,
//! but dropping one would drop a speculation guard.
//!
//! The pass iterates to a fixpoint: removing one instruction can make its
//! operands (a chain of dead pure ops) dead in turn.

use crate::opt::ir::{Function, Op, Term};

/// Drop every dead instruction in `func`. Returns whether anything changed.
pub fn run(func: &mut Function) -> bool {
    let mut changed = false;
    loop {
        let used = used_values(func);
        let mut removed = false;
        for b in 0..func.block_count() as u32 {
            let before = func.block(b).insts.len();
            func.block_mut(b).insts.retain(|inst| {
                let Some(result) = inst.result else {
                    // Effect-only: a store or a completion write.
                    return true;
                };
                if inst.op == Op::Check || inst.op == Op::GuardType || inst.op == Op::GuardCallee {
                    return true;
                }
                // A pure computation, or a frame load (a private-slot read that
                // cannot trap).
                if !inst.effects.is_pure() && inst.op != Op::FrameLoad {
                    return true;
                }
                used[result as usize]
            });
            removed |= func.block(b).insts.len() != before;
        }
        if !removed {
            return changed;
        }
        changed = true;
    }
}

/// The values referenced by any instruction argument, terminator condition, or
/// edge argument. Block parameters are definitions, not uses.
fn used_values(func: &Function) -> Vec<bool> {
    let mut used: Vec<bool> = vec![false; func.value_count() as usize];
    for b in 0..func.block_count() as u32 {
        let block = func.block(b);
        for inst in &block.insts {
            for &arg in &inst.args {
                used[arg as usize] = true;
            }
        }
        match block.term.as_ref() {
            Some(Term::Jump { args, .. }) => {
                for &arg in args {
                    used[arg as usize] = true;
                }
            }
            Some(Term::Branch {
                cond,
                then_args,
                else_args,
                ..
            }) => {
                used[*cond as usize] = true;
                for &arg in then_args.iter().chain(else_args) {
                    used[arg as usize] = true;
                }
            }
            Some(Term::Return(Some(v)) | Term::Throw(v)) => used[*v as usize] = true,
            _ => {}
        }
    }
    used
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::{Effects, Imm, Type, ValueId};

    fn const_num(b: &mut Builder<'_>, block: u32, v: f64) -> ValueId {
        b.emit(
            block,
            Op::Const,
            &[],
            Type::Number,
            Effects::pure(),
            Imm::Float(v),
        )
    }

    #[test]
    fn drops_an_unused_pure_value_and_keeps_a_used_one() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let dead = const_num(&mut b, entry, 1.0);
            let live = const_num(&mut b, entry, 2.0);
            let _ = dead;
            b.term(entry, Term::Return(Some(live)));
        }
        assert!(run(&mut func));
        assert_eq!(func.block(entry).insts.len(), 1);
        assert_eq!(func.block(entry).insts[0].imm, Imm::Float(2.0));
    }

    #[test]
    fn keeps_a_side_effecting_instruction_with_no_result() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let v = const_num(&mut b, entry, 1.0);
            b.emit_void(
                entry,
                Op::FrameStore,
                &[v],
                Effects::write(crate::opt::ir::Heap::Slots),
                Imm::Slot(0),
            );
            b.term(entry, Term::Return(None));
        }
        let _ = run(&mut func);
        // The store survives even though its result-less form is never "used".
        assert!(
            func.block(entry)
                .insts
                .iter()
                .any(|i| i.op == Op::FrameStore)
        );
    }

    #[test]
    fn keeps_an_unused_check_guard() {
        // `Check` is pure by `default_effects`, but dropping it would drop a
        // speculation guard, so the pass must keep it.
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let v = const_num(&mut b, entry, 1.0);
            b.emit(
                entry,
                Op::Check,
                &[v],
                Type::Bool,
                Effects::pure(),
                crate::opt::ir::Imm::None,
            );
            b.term(entry, Term::Return(None));
        }
        let _ = run(&mut func);
        assert!(func.block(entry).insts.iter().any(|i| i.op == Op::Check));
    }

    #[test]
    fn leaves_a_used_value_alone() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let v = const_num(&mut b, entry, 7.0);
            b.term(entry, Term::Return(Some(v)));
        }
        assert!(!run(&mut func));
        assert_eq!(func.block(entry).insts.len(), 1);
    }
}
