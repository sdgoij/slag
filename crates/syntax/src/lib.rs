//! Source text, tokens, spans, and the parse-node AST (spec ch. 12-16 syntax).
//!
//! Phase 2 defines `SourceText` (UTF-16) and `TokenKind`; Phase 3 adds the AST
//! node types with spans, interning identifiers via `crux::AtomId`.

#[cfg(test)]
mod tests {}
