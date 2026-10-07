//! The pass pipeline over the optimizing SSA IR (`.notes/optimizing-tier-impl.md`
//! §2, `pass/`).
//!
//! The pipeline is folding then dead-code elimination. Each pass iterates to its
//! own fixpoint internally, and one round suffices: folding turns ops into
//! constants (which exposes their former operands as dead), and DCE removes
//! only *unused* instructions, so it can never enable a fold. The caller
//! re-verifies the graph after the pipeline, because a pass bug must be a
//! refusal (the per-step path), never a wrong program.

use crate::opt::ir::Function;

pub mod dce;
pub mod fold;

/// Run the pass pipeline over `func`. Returns whether the IR changed.
pub fn run(func: &mut Function) -> bool {
    let folded = fold::run(func);
    let dropped = dce::run(func);
    folded || dropped
}
