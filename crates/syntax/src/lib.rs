//! Source text, tokens, spans, and the parse-node AST (spec ch. 12-16 syntax).
//!
//! Phase 2 defines `SourceText` (UTF-16), `TokenKind`, and the keyword tables;
//! Phase 3 adds the AST node types with spans, interning identifiers via
//! `crux::AtomId`.

pub mod ast;
pub mod keywords;
pub mod source;
pub mod token;

pub use ast::*;
pub use source::SourceText;
pub use token::{LexGoal, NumericLiteral, Token, TokenKind};
