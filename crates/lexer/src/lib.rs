//! Lexer implementing the lexical grammar: all goals, comments, literals, and
//! ASI input handling (spec ch. 11-12).

mod lexer;
mod numeric;
mod text;

pub use lexer::Lexer;
