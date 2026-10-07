//! The pass pipeline over the optimizing SSA IR (`.notes/optimizing-tier-impl.md`
//! §2, `pass/`).
//!
//! The pipeline is folding, numeric effect narrowing, CSE, then dead-code
//! elimination. Each pass and the pipeline as a whole is a fixpoint step; a
//! single round suffices because the passes are ordered so a later one never
//! enables an earlier one (folding creates constants, narrowing gives arithmetic
//! pure effects, CSE reuses them, DCE removes the leftovers). The caller
//! re-verifies the graph after the pipeline, because a pass bug must be a
//! refusal (the per-step path), never a wrong program.

use crate::opt::ir::Function;

pub mod cse;
pub mod dce;
pub mod fold;
pub mod narrow;

/// Run the pass pipeline over `func`. Returns whether the IR changed.
pub fn run(func: &mut Function) -> bool {
    let folded = fold::run(func);
    let narrowed = narrow::run(func);
    let cse = cse::run(func);
    let dropped = dce::run(func);
    folded || narrowed || cse || dropped
}
