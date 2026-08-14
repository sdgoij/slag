//! Tokens produced by the lexer (spec ch. 12).

use crux::{AtomId, BigInt, JsString, Span};

/// The active lexical goal symbol (spec 12.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexGoal {
    /// InputElementDiv: division and comments permitted.
    Div,
    /// InputElementRegExp: a `/` starts a RegularExpressionLiteral.
    RegExp,
    /// InputElementRegExpOrTemplateTail: regexp literals and template
    /// continuations (`}` + `` ` `` / `}` + `${`) permitted.
    RegExpOrTemplateTail,
    /// InputElementTemplateTail: only template continuations (no regexp).
    TemplateTail,
    /// InputElementHashbangOrRegExp: used at the start of a Script or Module.
    HashbangOrRegExp,
}

/// A numeric literal value (spec 12.9.3).
#[derive(Debug, Clone, PartialEq)]
pub enum NumericLiteral {
    Number(f64),
    BigInt(BigInt),
}

/// A lexical token.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Identifier(AtomId),
    PrivateIdentifier(AtomId),
    NullLiteral,
    BooleanLiteral(bool),
    NumericLiteral(NumericLiteral),
    StringLiteral {
        value: JsString,
        /// True when the literal contains an Annex B octal/NonOctal escape,
        /// which is an early error in strict mode.
        legacy_octal: bool,
    },
    NoSubstitutionTemplate {
        /// None when the template contains a NotEscapeSequence; only legal
        /// when the template is tagged.
        cooked: Option<JsString>,
        raw: JsString,
    },
    TemplateHead {
        cooked: Option<JsString>,
        raw: JsString,
    },
    TemplateMiddle {
        cooked: Option<JsString>,
        raw: JsString,
    },
    TemplateTail {
        cooked: Option<JsString>,
        raw: JsString,
    },
    RegExpLiteral {
        pattern: JsString,
        flags: JsString,
    },
    // Punctuators
    LeftParen,
    RightParen,
    LeftBrace,
    RightBrace,
    LeftBracket,
    RightBracket,
    Semicolon,
    Comma,
    Dot,
    Ellipsis,
    Question,
    QuestionDot,
    Colon,
    Arrow,
    PlusPlus,
    MinusMinus,
    Plus,
    Minus,
    Star,
    StarStar,
    Slash,
    Percent,
    LessThan,
    GreaterThan,
    LeftShift,
    RightShift,
    UnsignedRightShift,
    LessEqual,
    GreaterEqual,
    EqualEqual,
    NotEqual,
    StrictEqual,
    StrictNotEqual,
    Equal,
    PlusEqual,
    MinusEqual,
    StarEqual,
    StarStarEqual,
    SlashEqual,
    PercentEqual,
    LeftShiftEqual,
    RightShiftEqual,
    UnsignedRightShiftEqual,
    Ampersand,
    AmpersandEqual,
    And,
    AndEqual,
    Caret,
    CaretEqual,
    Pipe,
    PipeEqual,
    Or,
    OrEqual,
    NullishCoalescing,
    NullishCoalescingEqual,
    Not,
    Tilde,
    /// `@` — the decorator prefix (stage-3 decorators proposal).
    At,
    Eof,
}

/// A token with its source span and ASI-relevant line-break information.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// Whether a LineTerminator occurred before this token; drives ASI and
    /// the `[no LineTerminator here]` restrictions.
    pub line_break_before: bool,
    /// Whether an identifier token contained a `\u` escape sequence. Escaped
    /// contextual keywords are ordinary identifiers, and escaped reserved
    /// words are early errors (spec 12.6.1, 5.1.5).
    pub escaped: bool,
}
