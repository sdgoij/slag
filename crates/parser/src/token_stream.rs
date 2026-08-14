//! Token stream with lexical-goal management.
//!
//! The parser drives which `LexGoal` the lexer uses for each token (spec
//! 12.1): the goal flips between `Div` (after an expression — `/` is
//! division) and `RegExp` (at an expression start — `/` begins a literal).
//! Template continuations force the `TemplateTail` goal so the `}` closing a
//! substitution lexes as a `TemplateMiddle`/`TemplateTail` token.

use std::collections::VecDeque;

use crux::JsError;
use lexer::Lexer;
use syntax::{LexGoal, Token, TokenKind};

pub struct TokenStream<'s> {
    lexer: Lexer<'s>,
    /// Buffered tokens (up to two: `peek` and `peek2`).
    buffer: VecDeque<Token>,
    /// Lexer position at the start of each buffered token; rewinding here
    /// lets us re-lex under a different goal.
    starts: VecDeque<usize>,
    peeked_goal: LexGoal,
}

impl<'s> TokenStream<'s> {
    pub fn new(
        source: &'s syntax::SourceText,
        allow_html_comments: bool,
        allow_hashbang_after_directives: bool,
    ) -> Self {
        let mut lexer = Lexer::new(source, LexGoal::HashbangOrRegExp, allow_html_comments);
        if allow_hashbang_after_directives {
            lexer.set_allow_hashbang_after_directives();
        }
        Self {
            lexer,
            buffer: VecDeque::new(),
            starts: VecDeque::new(),
            peeked_goal: LexGoal::HashbangOrRegExp,
        }
    }

    /// Sets the lexical goal for the next token. If a token was already
    /// peeked under a different goal, it is re-lexed from its start so the
    /// new goal takes effect.
    pub fn set_goal(&mut self, goal: LexGoal) {
        if !self.buffer.is_empty() && self.peeked_goal != goal {
            if let Some(&start) = self.starts.front() {
                self.lexer.set_position(start);
            }
            self.buffer.clear();
            self.starts.clear();
        }
        self.peeked_goal = goal;
        self.lexer.set_goal(goal);
    }

    fn lex_one(&mut self) -> Result<Token, JsError> {
        self.starts.push_back(self.lexer.position());
        let token = self.lexer.next_token()?;
        self.buffer.push_back(token.clone());
        Ok(token)
    }

    pub fn peek(&mut self) -> Result<&Token, JsError> {
        if self.buffer.is_empty() {
            self.lex_one()?;
        }
        Ok(self.buffer.front().unwrap())
    }

    /// Peeks one token past the current one. The second token is lexed with
    /// the goal implied by the first token (division vs regexp), matching
    /// `Parser::next`.
    pub fn peek2(&mut self) -> Result<&Token, JsError> {
        self.peek()?;
        if self.buffer.len() < 2 {
            let goal = if can_end_expression(&self.buffer[0].kind) {
                LexGoal::Div
            } else {
                LexGoal::RegExp
            };
            self.lexer.set_goal(goal);
            self.lex_one()?;
        }
        Ok(&self.buffer[1])
    }

    /// Peeks two tokens past the current one (goal implied by the second
    /// token). Used for the `await using` disambiguation.
    pub fn peek3(&mut self) -> Result<&Token, JsError> {
        self.peek2()?;
        if self.buffer.len() < 3 {
            let goal = if can_end_expression(&self.buffer[1].kind) {
                LexGoal::Div
            } else {
                LexGoal::RegExp
            };
            self.lexer.set_goal(goal);
            self.lex_one()?;
        }
        Ok(&self.buffer[2])
    }

    pub fn next(&mut self) -> Result<Token, JsError> {
        self.peek()?;
        self.starts.pop_front();
        Ok(self.buffer.pop_front().unwrap())
    }

    /// A position-based snapshot for speculative parsing (the lexer is
    /// deterministic, so restoring the position re-lexes identically).
    pub fn snapshot(&self) -> usize {
        self.lexer.position()
    }

    pub fn restore(&mut self, snapshot: usize) {
        self.lexer.set_position(snapshot);
        self.buffer.clear();
        self.starts.clear();
    }
}

/// Whether a token can terminate an expression; drives the division-vs-regexp
/// lexical goal (spec 12.1). Keywords that are expression literals (`true`,
/// `false`, `null`, `this`) end expressions like any other literal; the other
/// keywords are prefixes or modifiers.
pub fn can_end_expression(kind: &TokenKind) -> bool {
    match kind {
        TokenKind::Identifier(atom) => match syntax::keywords::from_identifier(*atom) {
            None
            | Some(
                syntax::keywords::Keyword::True
                | syntax::keywords::Keyword::False
                | syntax::keywords::Keyword::Null
                | syntax::keywords::Keyword::This,
            ) => true,
            Some(_) => false,
        },
        TokenKind::PrivateIdentifier(_)
        | TokenKind::NullLiteral
        | TokenKind::BooleanLiteral(_)
        | TokenKind::NumericLiteral(_)
        | TokenKind::StringLiteral { .. }
        | TokenKind::NoSubstitutionTemplate { .. }
        | TokenKind::TemplateTail { .. }
        | TokenKind::RegExpLiteral { .. }
        | TokenKind::RightParen
        | TokenKind::RightBracket
        | TokenKind::RightBrace
        | TokenKind::PlusPlus
        | TokenKind::MinusMinus => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntax::{NumericLiteral, SourceText, TokenKind};

    fn stream<'s>(src: &'s SourceText) -> TokenStream<'s> {
        TokenStream::new(src, false, false)
    }

    #[test]
    fn yields_tokens_in_order() {
        let src = SourceText::from_utf8("a + 1");
        let mut s = stream(&src);
        let first = s.next().unwrap();
        assert!(matches!(first.kind, TokenKind::Identifier(_)));
        let second = s.next().unwrap();
        assert_eq!(second.kind, TokenKind::Plus);
        let third = s.next().unwrap();
        assert!(matches!(
            third.kind,
            TokenKind::NumericLiteral(NumericLiteral::Number(1.0))
        ));
    }

    #[test]
    fn goal_change_relexes_peeked_token() {
        // Peek `}` under the Div goal (RightBrace), then switch to
        // TemplateTail: the same source re-lexes as a template continuation.
        let src = SourceText::from_utf8("}tail`");
        let mut s = stream(&src);
        s.set_goal(LexGoal::Div);
        let t = s.peek().unwrap();
        assert_eq!(t.kind, TokenKind::RightBrace);
        s.set_goal(LexGoal::TemplateTail);
        let t = s.next().unwrap();
        assert!(matches!(t.kind, TokenKind::TemplateTail { .. }));
    }

    #[test]
    fn peek2_then_next_keeps_stream_consistent() {
        // After peek2, consuming the first token must expose the second.
        let src = SourceText::from_utf8("a b");
        let mut s = stream(&src);
        let t0 = s.peek().unwrap();
        assert!(matches!(t0.kind, TokenKind::Identifier(_)));
        let t1 = s.peek2().unwrap();
        assert!(matches!(t1.kind, TokenKind::Identifier(_)));
        let first = s.next().unwrap();
        assert!(matches!(first.kind, TokenKind::Identifier(_)));
        let second = s.peek().unwrap();
        assert!(matches!(second.kind, TokenKind::Identifier(_)));
        assert_eq!(second.span.start, 2);
    }

    #[test]
    fn eof_is_reachable() {
        let src = SourceText::from_utf8("x");
        let mut s = stream(&src);
        s.next().unwrap();
        let t = s.next().unwrap();
        assert_eq!(t.kind, TokenKind::Eof);
    }
}
