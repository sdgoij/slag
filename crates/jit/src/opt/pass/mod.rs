//! The pass pipeline over the optimizing SSA IR (`.notes/optimizing-tier-impl.md`
//! §2, `pass/`).
//!
//! The pipeline is folding, numeric effect narrowing, read-guard pruning, trial
//! inlining, CSE, LICM, then dead-code elimination. The order is chosen so a
//! single round reaches the fixpoint — a later pass never enables an earlier one
//! (folding creates constants, narrowing gives arithmetic pure effects, guard
//! pruning only removes, CSE reuses them, DCE removes the leftovers). Guard
//! pruning runs after narrowing because narrowing's numeric-slot proof *reads* a
//! guard (a slot stored only a guarded value is numeric); running it before would
//! let a store that feeds nothing type-sensitive lose the guard narrowing counted
//! on. Trial inlining is the one pass that can *create* work for an earlier one
//! (a cloned callee body may hold foldable arithmetic), so a body that inlines
//! may benefit from a second round — a missed fold in the inlined region is a
//! missed optimization, never a wrong program. Verified
//! empirically (`.notes/pass-pipeline-fixpoint.md`): iterating the pipeline to a
//! fixpoint performs no additional work on any sampled body, so the single round
//! is kept. The caller re-verifies the graph after the pipeline, because a pass
//! bug must be a refusal (the per-step path), never a wrong program.
//!
//! `mem2reg` is not part of this pipeline: it runs on a callee's lifted IR inside
//! the trial-inline resolver (I5c-2c-iii-b), so a spliced callee's `var` slots
//! have no frame home in the caller.

use crate::opt::ir::Function;

pub mod cse;
pub mod dce;
pub mod fold;
pub mod guard;
pub mod inline;
pub mod licm;
pub mod mem2reg;
pub mod narrow;

/// Whether the read-guard pruning pass runs. On by default; `SLAG_GUARD=0`
/// disables it — the same-binary seam for measuring the pass's marginal effect
/// (one executable, so no code-layout confound).
fn guard_enabled() -> bool {
    std::env::var("SLAG_GUARD")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Run the pass pipeline over `func`. Returns whether the IR changed. `sites`
/// is the caller's monomorphic call-site map and `resolve` yields a callee's
/// lifted IR (I5c-2c); an empty map makes the inline pass a no-op.
pub fn run(
    func: &mut Function,
    sites: &[Option<inline::InlineSite>],
    resolve: &mut dyn FnMut(u64) -> Option<inline::Callee>,
) -> bool {
    let folded = fold::run(func);
    let narrowed = narrow::run(func);
    let guarded = guard_enabled() && guard::run(func);
    let inlined = inline::run(func, sites, resolve);
    let cse = cse::run(func);
    let licm = licm::run(func);
    let dropped = dce::run(func);
    folded || narrowed || guarded || inlined || cse || licm || dropped
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
        assert!(
            run(&mut func, &[], &mut |_: u64| None),
            "the first round folds `1 + 2`"
        );
        assert!(
            !run(&mut func, &[], &mut |_: u64| None),
            "the second round finds nothing"
        );
    }
}
