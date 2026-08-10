//! ECMAScript RegExp pattern parser and backtracking matcher, used for literal
//! early errors and at runtime (spec ch. 21, Annex B).
//!
//! Phase 11 implements the pattern grammar per `u`/`v` flags, inline modifiers,
//! character-class set operations, and the backtracking engine.

#[cfg(test)]
mod tests {}
