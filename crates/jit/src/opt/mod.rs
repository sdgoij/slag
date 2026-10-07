//! The optimizing front end: a JS-level SSA control-flow IR, its builder and
//! verifier, and — later — the lift out of the interpreter's `Step` stream and
//! the passes that specialize a body.
//!
//! The module is deliberately Cranelift-free: the lowering lives in
//! `crate::opt_lower`, so the IR and its passes can be tested without a
//! backend. See `.notes/optimizing-tier-impl.md` for the layering and the
//! increment plan.
//!
//! Status: I0–I2 (the IR core, the lift, the lowering) plus the fold + DCE
//! pass pipeline.

pub mod builder;
pub mod ir;
pub mod lift;
pub mod pass;
pub mod print;
pub mod verify;

pub use ir::{BlockId, Effects, Function, Heap, Imm, Inst, Op, Term, Type, ValueId};
