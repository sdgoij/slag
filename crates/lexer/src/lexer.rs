//! The lexer core: trivia, identifiers, punctuators, and goal dispatch
//! (spec ch. 12).

use crux::{JsError, Span, intern};
use syntax::{LexGoal, Token, TokenKind};
use unicode::{
    is_identifier_part, is_identifier_start, is_line_terminator, is_unicode_id_continue,
    is_unicode_id_start, is_white_space,
};

use crate::text::TemplateKind;

/// Tokenizes `SourceText` per the active lexical goal.
pub struct Lexer<'s> {
    pub(crate) source: &'s syntax::SourceText,
    pub(crate) pos: usize,
    goal: LexGoal,
    allow_html_comments: bool,
    line_break_before: bool,
}

impl<'s> Lexer<'s> {
    pub fn new(source: &'s syntax::SourceText, goal: LexGoal, allow_html_comments: bool) -> Self {
        Self {
            source,
            pos: 0,
            goal,
            allow_html_comments,
            line_break_before: false,
        }
    }

    /// The parser switches the goal between tokens (division vs regexp vs
    /// template-tail contexts).
    pub fn set_goal(&mut self, goal: LexGoal) {
        self.goal = goal;
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub(crate) fn peek(&self) -> Option<u16> {
        self.source.code_unit(self.pos)
    }

    pub(crate) fn peek_n(&self, n: usize) -> Option<u16> {
        self.source.code_unit(self.pos + n)
    }

    pub(crate) fn source_units(&self, start: usize, end: usize) -> Vec<u16> {
        let start = start.min(self.source.len());
        let end = end.min(self.source.len());
        self.source.as_slice()[start.min(end)..end.max(start)].to_vec()
    }

    pub(crate) fn error_at(&self, start: usize, message: &str) -> JsError {
        JsError::new(crux::ErrorKind::SyntaxError, message.into())
            .with_span(Span::new(start as u32, self.pos as u32))
    }

    pub(crate) fn error_here(&self, message: &str) -> JsError {
        self.error_at(self.pos, message)
    }

    /// Produces the next token, consuming trivia (whitespace and comments).
    pub fn next_token(&mut self) -> Result<Token, JsError> {
        self.line_break_before = false;
        self.skip_trivia()?;
        let start = self.pos;
        let kind = match self.peek() {
            None => TokenKind::Eof,
            Some(u) => self.lex_token(u)?,
        };
        Ok(Token {
            kind,
            span: Span::new(start as u32, self.pos as u32),
            line_break_before: self.line_break_before,
        })
    }

    fn skip_trivia(&mut self) -> Result<(), JsError> {
        loop {
            let Some(u) = self.peek() else {
                return Ok(());
            };
            if is_white_space(u as u32) {
                self.pos += 1;
            } else if is_line_terminator(u as u32) {
                self.line_break_before = true;
                self.consume_line_terminator();
            } else if u == 0x2F {
                match self.peek_n(1) {
                    Some(0x2F) => {
                        self.pos += 2;
                        self.skip_line_comment();
                    }
                    Some(0x2A) => {
                        let start = self.pos;
                        self.pos += 2;
                        self.skip_block_comment(start)?;
                    }
                    _ => return Ok(()),
                }
            } else if u == 0x23
                && self.peek_n(1) == Some(0x21)
                && self.pos == 0
                && matches!(self.goal, LexGoal::HashbangOrRegExp)
            {
                // Hashbang comment at the very start of a Script or Module.
                self.pos += 2;
                self.skip_line_comment();
            } else if self.allow_html_comments
                && ((u == 0x3C
                    && self.peek_n(1) == Some(0x21)
                    && self.peek_n(2) == Some(0x2D)
                    && self.peek_n(3) == Some(0x2D))
                    || (u == 0x2D
                        && self.peek_n(1) == Some(0x2D)
                        && self.peek_n(2) == Some(0x3E)
                        && (self.pos == 0 || self.line_break_before)))
            {
                // Annex B HTML comments (script code only).
                self.pos += 3;
                if u == 0x3C {
                    self.pos += 1;
                }
                self.skip_line_comment();
            } else {
                return Ok(());
            }
        }
    }

    pub(crate) fn consume_line_terminator(&mut self) {
        if self.peek() == Some(0x000D) && self.peek_n(1) == Some(0x000A) {
            self.pos += 2;
        } else {
            self.pos += 1;
        }
    }

    fn skip_line_comment(&mut self) {
        while let Some(u) = self.peek() {
            if is_line_terminator(u as u32) {
                return;
            }
            self.pos += 1;
        }
    }

    fn skip_block_comment(&mut self, start: usize) -> Result<(), JsError> {
        loop {
            match self.peek() {
                None => return Err(self.error_at(start, "Unterminated comment")),
                Some(0x2A) if self.peek_n(1) == Some(0x2F) => {
                    self.pos += 2;
                    return Ok(());
                }
                Some(u) if is_line_terminator(u as u32) => {
                    self.line_break_before = true;
                    self.consume_line_terminator();
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn lex_token(&mut self, u: u16) -> Result<TokenKind, JsError> {
        match u {
            0x24 | 0x5F | 0x5C => self.lex_identifier(),
            u if is_identifier_start(u as u32) => self.lex_identifier(),
            0x23 => self.lex_private_identifier(),
            0x30..=0x39 => self.lex_numeric(),
            0x2E if self.peek_n(1).is_some_and(|x| (0x30..=0x39).contains(&x)) => {
                self.lex_numeric()
            }
            0x27 | 0x22 => self.lex_string(u),
            0x60 => self.lex_template(TemplateKind::Start),
            0x2F if matches!(
                self.goal,
                LexGoal::RegExp | LexGoal::RegExpOrTemplateTail | LexGoal::HashbangOrRegExp
            ) =>
            {
                self.lex_regexp()
            }
            0x7D if matches!(
                self.goal,
                LexGoal::TemplateTail | LexGoal::RegExpOrTemplateTail
            ) =>
            {
                // At a template-continuation position the `}` always starts a
                // TemplateMiddle or TemplateTail; the parser selects this goal
                // only where the grammar expects the continuation.
                self.lex_template(TemplateKind::Continuation)
            }
            _ => self.lex_punctuator(u),
        }
    }

    fn lex_identifier(&mut self) -> Result<TokenKind, JsError> {
        let start = self.pos;
        let mut units: Vec<u16> = Vec::new();
        let mut first = true;
        while let Some(u) = self.peek() {
            if u == b'\\' as u16 {
                let cp = self.lex_unicode_escape(start)?;
                let ok = if first {
                    is_unicode_id_start(cp)
                } else {
                    is_unicode_id_continue(cp)
                };
                if !ok {
                    return Err(self.error_at(start, "Invalid identifier escape"));
                }
                push_utf16(&mut units, cp);
            } else if (first && is_identifier_start(u as u32))
                || (!first && is_identifier_part(u as u32))
            {
                units.push(u);
                self.pos += 1;
            } else {
                break;
            }
            first = false;
        }
        Ok(TokenKind::Identifier(intern(&units)))
    }

    fn lex_private_identifier(&mut self) -> Result<TokenKind, JsError> {
        let start = self.pos;
        self.pos += 1; // '#'
        let mut units: Vec<u16> = Vec::new();
        let mut first = true;
        while let Some(u) = self.peek() {
            if u == b'\\' as u16 {
                let cp = self.lex_unicode_escape(start)?;
                let ok = if first {
                    is_unicode_id_start(cp)
                } else {
                    is_unicode_id_continue(cp)
                };
                if !ok {
                    return Err(self.error_at(start, "Invalid private identifier"));
                }
                push_utf16(&mut units, cp);
            } else if (first && is_identifier_start(u as u32))
                || (!first && is_identifier_part(u as u32))
            {
                units.push(u);
                self.pos += 1;
            } else {
                break;
            }
            first = false;
        }
        if units.is_empty() {
            return Err(self.error_at(start, "Invalid private identifier"));
        }
        Ok(TokenKind::PrivateIdentifier(intern(&units)))
    }

    /// Consumes `\uXXXX` or `\u{...}` and returns the code point.
    fn lex_unicode_escape(&mut self, start: usize) -> Result<u32, JsError> {
        if self.peek() != Some(b'\\' as u16) || self.peek_n(1) != Some(0x75) {
            return Err(self.error_at(start, "Invalid escape sequence"));
        }
        self.pos += 2;
        if self.peek() == Some(0x7B) {
            self.pos += 1;
            let mut value: u32 = 0;
            let mut digits = 0;
            loop {
                match self.peek() {
                    Some(u) if is_hex_digit(u) => {
                        value = value * 16 + hex_value(u);
                        self.pos += 1;
                        digits += 1;
                        if digits > 6 {
                            return Err(self.error_at(start, "Invalid Unicode escape"));
                        }
                    }
                    Some(0x7D) => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(self.error_at(start, "Invalid Unicode escape")),
                }
            }
            if digits == 0 || value > 0x10FFFF {
                return Err(self.error_at(start, "Invalid Unicode escape"));
            }
            Ok(value)
        } else {
            let mut value: u32 = 0;
            for _ in 0..4 {
                let u = self
                    .peek()
                    .ok_or_else(|| self.error_at(start, "Invalid Unicode escape"))?;
                if !is_hex_digit(u) {
                    return Err(self.error_at(start, "Invalid Unicode escape"));
                }
                value = value * 16 + hex_value(u);
                self.pos += 1;
            }
            Ok(value)
        }
    }

    fn lex_punctuator(&mut self, u: u16) -> Result<TokenKind, JsError> {
        let next = self.peek_n(1);
        let next2 = self.peek_n(2);
        let next3 = self.peek_n(3);
        let is_digit = |x: u16| (0x30..=0x39).contains(&x);
        let kind = match u {
            0x3E => {
                if next == Some(0x3E) {
                    if next2 == Some(0x3E) {
                        if next3 == Some(0x3D) {
                            self.pos += 4;
                            TokenKind::UnsignedRightShiftEqual
                        } else {
                            self.pos += 3;
                            TokenKind::UnsignedRightShift
                        }
                    } else if next2 == Some(0x3D) {
                        self.pos += 3;
                        TokenKind::RightShiftEqual
                    } else {
                        self.pos += 2;
                        TokenKind::RightShift
                    }
                } else if next == Some(0x3D) {
                    self.pos += 2;
                    TokenKind::GreaterEqual
                } else {
                    self.pos += 1;
                    TokenKind::GreaterThan
                }
            }
            0x3C => {
                if next == Some(0x3C) {
                    if next2 == Some(0x3D) {
                        self.pos += 3;
                        TokenKind::LeftShiftEqual
                    } else {
                        self.pos += 2;
                        TokenKind::LeftShift
                    }
                } else if next == Some(0x3D) {
                    self.pos += 2;
                    TokenKind::LessEqual
                } else {
                    self.pos += 1;
                    TokenKind::LessThan
                }
            }
            0x3D => {
                if next == Some(0x3D) {
                    if next2 == Some(0x3D) {
                        self.pos += 3;
                        TokenKind::StrictEqual
                    } else {
                        self.pos += 2;
                        TokenKind::EqualEqual
                    }
                } else if next == Some(0x3E) {
                    self.pos += 2;
                    TokenKind::Arrow
                } else {
                    self.pos += 1;
                    TokenKind::Equal
                }
            }
            0x21 => {
                if next == Some(0x3D) {
                    if next2 == Some(0x3D) {
                        self.pos += 3;
                        TokenKind::StrictNotEqual
                    } else {
                        self.pos += 2;
                        TokenKind::NotEqual
                    }
                } else {
                    self.pos += 1;
                    TokenKind::Not
                }
            }
            0x2A => {
                if next == Some(0x2A) {
                    if next2 == Some(0x3D) {
                        self.pos += 3;
                        TokenKind::StarStarEqual
                    } else {
                        self.pos += 2;
                        TokenKind::StarStar
                    }
                } else if next == Some(0x3D) {
                    self.pos += 2;
                    TokenKind::StarEqual
                } else {
                    self.pos += 1;
                    TokenKind::Star
                }
            }
            0x26 => {
                if next == Some(0x26) {
                    if next2 == Some(0x3D) {
                        self.pos += 3;
                        TokenKind::AmpersandEqual
                    } else {
                        self.pos += 2;
                        TokenKind::And
                    }
                } else if next == Some(0x3D) {
                    self.pos += 2;
                    TokenKind::AmpersandEqual
                } else {
                    self.pos += 1;
                    TokenKind::Ampersand
                }
            }
            0x7C => {
                if next == Some(0x7C) {
                    if next2 == Some(0x3D) {
                        self.pos += 3;
                        TokenKind::PipeEqual
                    } else {
                        self.pos += 2;
                        TokenKind::Or
                    }
                } else if next == Some(0x3D) {
                    self.pos += 2;
                    TokenKind::PipeEqual
                } else {
                    self.pos += 1;
                    TokenKind::Pipe
                }
            }
            0x3F => {
                if next == Some(0x3F) {
                    if next2 == Some(0x3D) {
                        self.pos += 3;
                        TokenKind::NullishCoalescingEqual
                    } else {
                        self.pos += 2;
                        TokenKind::NullishCoalescing
                    }
                } else if next == Some(0x2E) && !self.peek_n(2).is_some_and(is_digit) {
                    self.pos += 2;
                    TokenKind::QuestionDot
                } else {
                    self.pos += 1;
                    TokenKind::Question
                }
            }
            0x2B => {
                if next == Some(0x2B) {
                    self.pos += 2;
                    TokenKind::PlusPlus
                } else if next == Some(0x3D) {
                    self.pos += 2;
                    TokenKind::PlusEqual
                } else {
                    self.pos += 1;
                    TokenKind::Plus
                }
            }
            0x2D => {
                if next == Some(0x2D) {
                    self.pos += 2;
                    TokenKind::MinusMinus
                } else if next == Some(0x3D) {
                    self.pos += 2;
                    TokenKind::MinusEqual
                } else {
                    self.pos += 1;
                    TokenKind::Minus
                }
            }
            0x25 => {
                if next == Some(0x3D) {
                    self.pos += 2;
                    TokenKind::PercentEqual
                } else {
                    self.pos += 1;
                    TokenKind::Percent
                }
            }
            0x2F => {
                if next == Some(0x3D) {
                    self.pos += 2;
                    TokenKind::SlashEqual
                } else {
                    self.pos += 1;
                    TokenKind::Slash
                }
            }
            0x5E => {
                if next == Some(0x3D) {
                    self.pos += 2;
                    TokenKind::CaretEqual
                } else {
                    self.pos += 1;
                    TokenKind::Caret
                }
            }
            0x7E => {
                self.pos += 1;
                TokenKind::Tilde
            }
            0x2E => {
                if next == Some(0x2E) && next2 == Some(0x2E) {
                    self.pos += 3;
                    TokenKind::Ellipsis
                } else {
                    self.pos += 1;
                    TokenKind::Dot
                }
            }
            0x28 => {
                self.pos += 1;
                TokenKind::LeftParen
            }
            0x29 => {
                self.pos += 1;
                TokenKind::RightParen
            }
            0x7B => {
                self.pos += 1;
                TokenKind::LeftBrace
            }
            0x7D => {
                self.pos += 1;
                TokenKind::RightBrace
            }
            0x5B => {
                self.pos += 1;
                TokenKind::LeftBracket
            }
            0x5D => {
                self.pos += 1;
                TokenKind::RightBracket
            }
            0x3B => {
                self.pos += 1;
                TokenKind::Semicolon
            }
            0x2C => {
                self.pos += 1;
                TokenKind::Comma
            }
            0x3A => {
                self.pos += 1;
                TokenKind::Colon
            }
            _ => return Err(self.error_here("Unexpected character")),
        };
        Ok(kind)
    }
}

/// UTF16EncodeCodePoint for a single code point.
fn push_utf16(out: &mut Vec<u16>, cp: u32) {
    if cp <= 0xFFFF {
        out.push(cp as u16);
    } else {
        let x = cp - 0x10000;
        out.push(0xD800 + (x >> 10) as u16);
        out.push(0xDC00 + (x & 0x3FF) as u16);
    }
}

fn is_hex_digit(u: u16) -> bool {
    (0x30..=0x39).contains(&u) || (0x61..=0x66).contains(&u) || (0x41..=0x46).contains(&u)
}

fn hex_value(u: u16) -> u32 {
    match u {
        0x30..=0x39 => (u - 0x30) as u32,
        0x61..=0x66 => (u - 0x61 + 10) as u32,
        0x41..=0x46 => (u - 0x41 + 10) as u32,
        _ => unreachable!("validated by is_hex_digit"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crux::BigInt;
    use syntax::NumericLiteral;

    fn lex(source: &str) -> Vec<Token> {
        let src = syntax::SourceText::from_utf8(source);
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        let mut tokens = Vec::new();
        loop {
            let token = lexer.next_token().expect("lexing must not fail");
            let done = token.kind == TokenKind::Eof;
            tokens.push(token);
            if done {
                break;
            }
        }
        tokens
    }

    fn kinds(source: &str) -> Vec<TokenKind> {
        lex(source).into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn skips_whitespace_and_comments() {
        let src = syntax::SourceText::from_utf8("// line\n/* block */\t\ra");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        let token = lexer.next_token().unwrap();
        assert_eq!(token.kind, TokenKind::Identifier(intern(&[0x61])));
        assert!(token.line_break_before);
    }

    #[test]
    fn hashbang_is_consumed_at_start() {
        let src = syntax::SourceText::from_utf8("#!/usr/bin/env node\n1");
        let mut lexer = Lexer::new(&src, LexGoal::HashbangOrRegExp, false);
        let token = lexer.next_token().unwrap();
        assert_eq!(
            token.kind,
            TokenKind::NumericLiteral(NumericLiteral::Number(1.0))
        );
        assert!(token.line_break_before);
    }

    #[test]
    fn html_comments_are_annex_b_conditional() {
        let src = syntax::SourceText::from_utf8("<!--x\n-->\ny");
        let mut lexer = Lexer::new(&src, LexGoal::Div, true);
        let t1 = lexer.next_token().unwrap();
        assert!(matches!(t1.kind, TokenKind::Identifier(_)));
        assert!(t1.line_break_before);
        let t2 = lexer.next_token().unwrap();
        assert_eq!(t2.kind, TokenKind::Eof);
    }

    #[test]
    fn identifiers_with_escapes_and_unicode() {
        assert_eq!(
            kinds("abc $ _"),
            vec![
                TokenKind::Identifier(intern(&[0x61, 0x62, 0x63])),
                TokenKind::Identifier(intern(&[0x24])),
                TokenKind::Identifier(intern(&[0x5F])),
                TokenKind::Eof,
            ]
        );
        // \u{1F600} is not an ID_Start → error.
        let src = syntax::SourceText::from_utf8("\\u{1F600}");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        assert!(lexer.next_token().is_err());
        // \u0061 → 'a'
        let src = syntax::SourceText::from_utf8("\\u0061\\u{62}");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        let token = lexer.next_token().unwrap();
        assert_eq!(token.kind, TokenKind::Identifier(intern(&[0x61, 0x62])));
        // CJK identifier
        let src = syntax::SourceText::from_utf8("变量");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        let token = lexer.next_token().unwrap();
        assert_eq!(token.kind, TokenKind::Identifier(intern(&[0x53D8, 0x91CF])));
    }

    #[test]
    fn private_identifiers() {
        assert_eq!(
            kinds("#x #\u{5F}"),
            vec![
                TokenKind::PrivateIdentifier(intern(&[0x78])),
                TokenKind::PrivateIdentifier(intern(&[0x5F])),
                TokenKind::Eof,
            ]
        );
        let src = syntax::SourceText::from_utf8("#");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        assert!(lexer.next_token().is_err());
    }

    #[test]
    fn numeric_literals_all_forms() {
        assert_eq!(
            kinds("0 42 2.25 .5 5. 1e3 1E-2"),
            vec![
                TokenKind::NumericLiteral(NumericLiteral::Number(0.0)),
                TokenKind::NumericLiteral(NumericLiteral::Number(42.0)),
                TokenKind::NumericLiteral(NumericLiteral::Number(2.25)),
                TokenKind::NumericLiteral(NumericLiteral::Number(0.5)),
                TokenKind::NumericLiteral(NumericLiteral::Number(5.0)),
                TokenKind::NumericLiteral(NumericLiteral::Number(1000.0)),
                TokenKind::NumericLiteral(NumericLiteral::Number(0.01)),
                TokenKind::Eof,
            ]
        );
        assert_eq!(
            kinds("0x1F 0o17 0b101 089 0123 1_000_000"),
            vec![
                TokenKind::NumericLiteral(NumericLiteral::Number(31.0)),
                TokenKind::NumericLiteral(NumericLiteral::Number(15.0)),
                TokenKind::NumericLiteral(NumericLiteral::Number(5.0)),
                TokenKind::NumericLiteral(NumericLiteral::Number(89.0)),
                TokenKind::NumericLiteral(NumericLiteral::Number(83.0)), // legacy octal
                TokenKind::NumericLiteral(NumericLiteral::Number(1_000_000.0)),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn bigint_literals() {
        let toks = kinds("123n 0xFn 1_000n");
        assert_eq!(
            toks[0],
            TokenKind::NumericLiteral(NumericLiteral::BigInt(BigInt::from(123)))
        );
        assert_eq!(
            toks[1],
            TokenKind::NumericLiteral(NumericLiteral::BigInt(BigInt::from(15)))
        );
        assert_eq!(
            toks[2],
            TokenKind::NumericLiteral(NumericLiteral::BigInt(BigInt::from(1000)))
        );
    }

    #[test]
    fn numeric_literal_errors() {
        for bad in ["0x", "0x_1", "1__0", "1_", "0b", "0123.5", "00.5"] {
            let src = syntax::SourceText::from_utf8(bad);
            let mut lexer = Lexer::new(&src, LexGoal::Div, false);
            assert!(lexer.next_token().is_err(), "expected error for {bad}");
        }
        // `0e5` is a valid zero with an exponent.
        let src = syntax::SourceText::from_utf8("0e5");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        let token = lexer.next_token().unwrap();
        assert_eq!(
            token.kind,
            TokenKind::NumericLiteral(NumericLiteral::Number(0.0))
        );
    }

    #[test]
    fn punctuators_longest_match() {
        assert_eq!(
            kinds(
                ">>> >>>= >> >= <<= << <= == === != !== && &&= || ||= ?? ??= ?. ** **= => ... ++ -- += -= *= /= %= &= |= ^= ! ~"
            ),
            vec![
                TokenKind::UnsignedRightShift,
                TokenKind::UnsignedRightShiftEqual,
                TokenKind::RightShift,
                TokenKind::GreaterEqual,
                TokenKind::LeftShiftEqual,
                TokenKind::LeftShift,
                TokenKind::LessEqual,
                TokenKind::EqualEqual,
                TokenKind::StrictEqual,
                TokenKind::NotEqual,
                TokenKind::StrictNotEqual,
                TokenKind::And,
                TokenKind::AmpersandEqual,
                TokenKind::Or,
                TokenKind::PipeEqual,
                TokenKind::NullishCoalescing,
                TokenKind::NullishCoalescingEqual,
                TokenKind::QuestionDot,
                TokenKind::StarStar,
                TokenKind::StarStarEqual,
                TokenKind::Arrow,
                TokenKind::Ellipsis,
                TokenKind::PlusPlus,
                TokenKind::MinusMinus,
                TokenKind::PlusEqual,
                TokenKind::MinusEqual,
                TokenKind::StarEqual,
                TokenKind::SlashEqual,
                TokenKind::PercentEqual,
                TokenKind::AmpersandEqual,
                TokenKind::PipeEqual,
                TokenKind::CaretEqual,
                TokenKind::Not,
                TokenKind::Tilde,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn question_dot_not_before_digit() {
        // `?.` followed by a digit is `?` then `.5`.
        assert_eq!(
            kinds("a ? .5 : b"),
            vec![
                TokenKind::Identifier(intern(&[0x61])),
                TokenKind::Question,
                TokenKind::NumericLiteral(NumericLiteral::Number(0.5)),
                TokenKind::Colon,
                TokenKind::Identifier(intern(&[0x62])),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn unterminated_constructs_error() {
        for bad in ["\"abc", "'abc", "/* abc", "/abc", "`abc"] {
            let src = syntax::SourceText::from_utf8(bad);
            let mut lexer = Lexer::new(&src, LexGoal::RegExp, false);
            assert!(lexer.next_token().is_err(), "expected error for {bad}");
        }
    }

    #[test]
    fn eof_token_has_empty_span() {
        let toks = lex("");
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, TokenKind::Eof);
        assert_eq!(toks[0].span, Span::empty(0));
    }

    #[test]
    fn spans_cover_token_text() {
        let src = syntax::SourceText::from_utf8("let x = 42;");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        let token = lexer.next_token().unwrap();
        assert_eq!(token.span, Span::new(0, 3));
        assert_eq!(
            src.substring(token.span.start, token.span.end)
                .to_string_lossy(),
            "let"
        );
    }

    #[test]
    fn line_break_before_covers_all_terminator_forms() {
        for sep in ["\n", "\r", "\r\n", "\u{2028}", "\u{2029}"] {
            let src = syntax::SourceText::from_utf8(&format!("1{sep}2"));
            let mut lexer = Lexer::new(&src, LexGoal::Div, false);
            assert!(!lexer.next_token().unwrap().line_break_before);
            assert!(
                lexer.next_token().unwrap().line_break_before,
                "expected a break before the token after {sep:?}"
            );
        }
    }

    #[test]
    fn line_break_via_comments() {
        // A line comment ends at the line terminator.
        let src = syntax::SourceText::from_utf8("// c\n1");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        assert!(lexer.next_token().unwrap().line_break_before);
        // A block comment with an internal newline counts as a break.
        let src = syntax::SourceText::from_utf8("/* a\nb */1");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        assert!(lexer.next_token().unwrap().line_break_before);
        // A block comment without newlines does not.
        let src = syntax::SourceText::from_utf8("/* a */1");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        assert!(!lexer.next_token().unwrap().line_break_before);
        // CRLF inside a block comment is a single break.
        let src = syntax::SourceText::from_utf8("/* a\r\nb */1");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        assert!(lexer.next_token().unwrap().line_break_before);
    }

    #[test]
    fn slash_goal_dispatches_division_vs_regexp() {
        // Division goal: `/` and `/=` are punctuators.
        let src = syntax::SourceText::from_utf8("/= x");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        assert_eq!(lexer.next_token().unwrap().kind, TokenKind::SlashEqual);
        // RegExp goal: `/` starts a literal even when it begins with `*`-
        // adjacent text such as `/*/` (handled as a comment in Div goal).
        let src = syntax::SourceText::from_utf8("/a+/");
        let mut lexer = Lexer::new(&src, LexGoal::RegExp, false);
        assert!(matches!(
            lexer.next_token().unwrap().kind,
            TokenKind::RegExpLiteral { .. }
        ));
        // HashbangOrRegExp also enables regexp literals.
        let src = syntax::SourceText::from_utf8("/x/");
        let mut lexer = Lexer::new(&src, LexGoal::HashbangOrRegExp, false);
        assert!(matches!(
            lexer.next_token().unwrap().kind,
            TokenKind::RegExpLiteral { .. }
        ));
    }

    #[test]
    fn template_tail_goal_continuation() {
        // In TemplateTail/RegExpOrTemplateTail goals a leading `}` starts a
        // template continuation instead of a RightBrace punctuator.
        let src = syntax::SourceText::from_utf8("}tail`");
        let mut lexer = Lexer::new(&src, LexGoal::TemplateTail, false);
        assert!(matches!(
            lexer.next_token().unwrap().kind,
            TokenKind::TemplateTail { .. }
        ));
        // The same source in the Div goal yields a RightBrace.
        let src = syntax::SourceText::from_utf8("}tail`");
        let mut lexer = Lexer::new(&src, LexGoal::Div, false);
        assert_eq!(lexer.next_token().unwrap().kind, TokenKind::RightBrace);
    }
}
