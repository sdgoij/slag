//! The pass pipeline over the optimizing SSA IR (`.notes/optimizing-tier-impl.md`
//! §2, `pass/`).
//!
//! The pipeline is folding, numeric effect narrowing, CSE, LICM, then dead-code
//! elimination. The order is chosen so a single round reaches the fixpoint — a
//! later pass never enables an earlier one (folding creates constants, narrowing
//! gives arithmetic pure effects, CSE reuses them, DCE removes the leftovers).
//! Verified empirically (`.notes/pass-pipeline-fixpoint.md`): iterating the
//! pipeline to a fixpoint performs no additional work on any sampled body, so the
//! single round is kept. The caller re-verifies the graph after the pipeline,
//! because a pass bug must be a refusal (the per-step path), never a wrong
//! program.

use crate::opt::ir::Function;

pub mod cse;
pub mod dce;
pub mod fold;
pub mod licm;
pub mod narrow;

/// Run the pass pipeline over `func`. Returns whether the IR changed.
pub fn run(func: &mut Function) -> bool {
    let folded = fold::run(func);
    let narrowed = narrow::run(func);
    let cse = cse::run(func);
    let licm = licm::run(func);
    let dropped = dce::run(func);
    folded || narrowed || cse || licm || dropped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::builder::Builder;
    use crate::opt::ir::{Effects, Imm, Op, Term, Type};

    #[test]
    fn the_pipeline_reaches_its_fixpoint_in_one_round() {
        // `1 + 2` folds, so round 1 changes the IR; a second round must find
        // nothing — the ordering invariant the module doc claims (no later pass
        // enables an earlier one). A body where a second round *did* change
        // something would refute the claim.
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let one = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(1.0),
            );
            let two = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Number,
                Effects::pure(),
                Imm::Float(2.0),
            );
            let sum = b.emit(
                entry,
                Op::Add,
                &[one, two],
                Type::Unknown,
                Effects::call(),
                Imm::None,
            );
            b.term(entry, Term::Return(Some(sum)));
        }
        assert!(run(&mut func), "the first round folds `1 + 2`");
        assert!(!run(&mut func), "the second round finds nothing");
    }
}
