//! Recursive-descent parser: syntactic grammar, cover grammar, all early
//! errors, and ASI (spec ch. 13-17).
//!
//! Phase 3 implements the parameterized grammar `[Yield, Await, Return, In]`
//! and the syntax-directed operations the evaluator needs.

#[cfg(test)]
mod tests {}
