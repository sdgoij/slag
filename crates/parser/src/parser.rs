//! The parser core: token helpers, ASI, context flags, and early-error
//! scope tracking (spec ch. 13-17).

use std::collections::HashSet;

use crux::{AtomId, JsError, Span, intern_utf8};
use syntax::keywords::{Keyword, from_identifier, is_future_reserved_word};
use syntax::{
    ArrayBindingElement, AssignOp, BindingElement, BindingPattern, Expr, ExprKind,
    ObjectBindingProperty, PropertyName, Token, TokenKind,
};

use crate::token_stream::{TokenStream, can_end_expression};

/// A label in scope; records pending `continue label` statements that are
/// validated once the labeled statement's shape is known.
pub(crate) struct LabelInfo {
    pub(crate) name: AtomId,
    /// Spans of `continue label` statements that need re-validation once the
    /// labeled statement's shape is known.
    pub(crate) pending_continues: Vec<u32>,
}

/// One lexical scope; tracks `let`/`const`/class names, function-declared
/// names, and parameter names for duplicate declaration early errors.
#[derive(Default)]
pub(crate) struct Scope {
    pub(crate) lexical: HashSet<AtomId>,
    /// Names declared by function declarations in this scope (var/function
    /// coexistence rules).
    pub(crate) functions: HashSet<AtomId>,
    /// Formal-parameter names (clash with body `let`/`const` only).
    pub(crate) params: HashSet<AtomId>,
    /// True for the scope of a function body: `var` names do not escape it.
    pub(crate) is_function: bool,
}

pub struct Parser<'s> {
    pub(crate) stream: TokenStream<'s>,
    /// The source text, for re-materializing raw slices (e.g. directives).
    pub(crate) source: &'s syntax::SourceText,
    /// The last consumed token (used for the lexical goal and ASI).
    pub(crate) prev: Option<Token>,

    // Context parameters of the grammar ([Yield, Await, Return]).
    pub(crate) strict: bool,
    pub(crate) in_function: bool,
    pub(crate) in_generator: bool,
    pub(crate) in_async: bool,
    /// Inside arrow-function parameter cover grammar; `{a = 1}` shorthand
    /// initializers are only legal here (spec 13.2.5 CoverInitializedName).
    pub(crate) in_arrow_cover: bool,
    /// First CoverInitializedName parsed while `in_arrow_cover`, if the
    /// enclosing parenthesized list never disambiguates into an arrow.
    pub(crate) cover_error: Option<Span>,

    // Early-error tracking.
    pub(crate) scopes: Vec<Scope>,
    /// `var` names declared in the current statement list (per the
    /// lexical-vs-var disjointness rule).
    pub(crate) list_vars: HashSet<AtomId>,
    pub(crate) labels: Vec<LabelInfo>,
    pub(crate) loop_depth: usize,
    pub(crate) switch_depth: usize,
}

/// A parsed parenthesized list: each element is either a plain expression or
/// an arrow-parameter-only form (`...rest`).
pub(crate) enum ParenItem {
    Expr(Expr),
    /// `...expr` — rest parameter (arrow) or an error (plain parens).
    Spread(Expr),
}

/// The outcome of parsing `( … )` at an expression start.
pub(crate) enum ParenResult {
    /// `()` — empty parentheses.
    Empty,
    /// A parenthesized expression (may wrap a Sequence).
    Expr(Expr),
    /// Followed by `=>`: the disambiguated arrow parameter list. The caller
    /// parses the arrow body.
    ArrowParams(Vec<BindingElement>),
}

impl<'s> Parser<'s> {
    pub fn new(source: &'s syntax::SourceText, allow_html_comments: bool) -> Self {
        Self {
            stream: TokenStream::new(source, allow_html_comments),
            source,
            prev: None,
            strict: false,
            in_function: false,
            in_generator: false,
            in_async: false,
            in_arrow_cover: false,
            cover_error: None,
            scopes: vec![Scope::default()],
            list_vars: HashSet::new(),
            labels: Vec::new(),
            loop_depth: 0,
            switch_depth: 0,
        }
    }

    /// The UTF-16 code units covered by `span`.
    pub(crate) fn source_slice(&self, span: Span) -> Vec<u16> {
        let start = span.start as usize;
        let end = (span.end as usize).min(self.source.len());
        self.source.as_slice()[start.min(end)..end].to_vec()
    }

    // ---- tokens ----

    /// Peeks the next token, first applying the lexical goal implied by the
    /// previous token (division vs regexp).
    pub(crate) fn peek(&mut self) -> Result<&Token, JsError> {
        let goal = match &self.prev {
            None => syntax::LexGoal::HashbangOrRegExp,
            Some(prev) if can_end_expression(&prev.kind) => syntax::LexGoal::Div,
            Some(_) => syntax::LexGoal::RegExp,
        };
        self.stream.set_goal(goal);
        self.stream.peek()
    }

    pub(crate) fn peek2(&mut self) -> Result<&Token, JsError> {
        self.stream.peek2()
    }

    pub(crate) fn snapshot(&self) -> usize {
        self.stream.snapshot()
    }

    pub(crate) fn restore(&mut self, snapshot: usize) {
        self.stream.restore(snapshot);
        self.prev = None;
    }

    /// Consumes the next token, choosing the lexical goal from the previous
    /// token (division vs regexp).
    pub(crate) fn next(&mut self) -> Result<Token, JsError> {
        self.peek()?;
        let token = self.stream.next()?;
        self.prev = Some(token.clone());
        Ok(token)
    }

    /// Consumes the next token with an explicit lexical goal (used for
    /// template continuations).
    pub(crate) fn next_with_goal(&mut self, goal: syntax::LexGoal) -> Result<Token, JsError> {
        self.stream.set_goal(goal);
        let token = self.stream.next()?;
        self.prev = Some(token.clone());
        Ok(token)
    }

    pub(crate) fn at_punct(&mut self, kind: TokenKind) -> Result<bool, JsError> {
        Ok(self.peek()?.kind == kind)
    }

    pub(crate) fn eat_punct(&mut self, kind: TokenKind) -> Result<bool, JsError> {
        if self.at_punct(kind.clone())? {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(crate) fn expect_punct(&mut self, kind: TokenKind) -> Result<(), JsError> {
        if self.eat_punct(kind.clone())? {
            Ok(())
        } else {
            let tok = self.peek()?.clone();
            Err(self.unexpected(&tok))
        }
    }

    pub(crate) fn at_keyword(&mut self, kw: Keyword) -> Result<bool, JsError> {
        let TokenKind::Identifier(atom) = self.peek()?.kind else {
            return Ok(false);
        };
        Ok(from_identifier(atom) == Some(kw))
    }

    pub(crate) fn eat_keyword(&mut self, kw: Keyword) -> Result<bool, JsError> {
        if self.at_keyword(kw)? {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(crate) fn expect_keyword(&mut self, kw: Keyword) -> Result<(), JsError> {
        if self.eat_keyword(kw)? {
            Ok(())
        } else {
            let tok = self.peek()?.clone();
            Err(self.unexpected(&tok))
        }
    }

    /// Whether the current token is the identifier with the given text
    /// (contextual keywords such as `async`, `of`, `get`, `set`).
    pub(crate) fn at_contextual(&mut self, text: &str) -> Result<bool, JsError> {
        let id = intern_utf8(text);
        Ok(self.peek()?.kind == TokenKind::Identifier(id))
    }

    /// Whether the current token is an identifier that is not a keyword and
    /// is legal to bind in the current strictness.
    pub(crate) fn at_identifier(&mut self) -> Result<bool, JsError> {
        let TokenKind::Identifier(atom) = self.peek()?.kind else {
            return Ok(false);
        };
        if from_identifier(atom).is_some() {
            return Ok(false);
        }
        if self.strict && is_future_reserved_word(atom) {
            return Ok(false);
        }
        if self.in_generator && atom == intern_utf8("yield") {
            return Ok(false);
        }
        if self.in_async && atom == intern_utf8("await") {
            return Ok(false);
        }
        Ok(true)
    }

    // ---- errors and ASI ----

    pub(crate) fn unexpected(&self, tok: &Token) -> JsError {
        let message = match &tok.kind {
            TokenKind::Eof => "Unexpected end of input".to_string(),
            _ => "Unexpected token".to_string(),
        };
        self.error_at(tok.span.start, &message)
    }

    pub(crate) fn error_at(&self, start: u32, message: &str) -> JsError {
        JsError::new(crux::ErrorKind::SyntaxError, message.into())
            .with_span(Span::new(start, start))
    }

    /// Consumes a semicolon, applying ASI: a `;` may be omitted before `}`,
    /// at end of input, or after a line terminator (spec 12.10).
    pub(crate) fn expect_semicolon(&mut self) -> Result<(), JsError> {
        if self.eat_punct(TokenKind::Semicolon)? {
            return Ok(());
        }
        let tok = self.peek()?.clone();
        match tok.kind {
            TokenKind::RightBrace | TokenKind::Eof => Ok(()),
            _ if tok.line_break_before => Ok(()),
            _ => Err(self.unexpected(&tok)),
        }
    }

    // ---- scopes and early errors ----

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Declares a `let`/`const`/`class` name: duplicates in the same scope,
    /// clashes with `var`/function names in the same statement list, and
    /// clashes with a formal parameter are early errors.
    pub(crate) fn declare_lexical(&mut self, name: AtomId, start: u32) -> Result<(), JsError> {
        let scope = self.scopes.last_mut().unwrap();
        if scope.lexical.contains(&name)
            || scope.params.contains(&name)
            || self.list_vars.contains(&name)
        {
            return Err(self.error_at(start, "Identifier has already been declared"));
        }
        scope.lexical.insert(name);
        Ok(())
    }

    /// Declares a `var` name: it may repeat, but must not clash with a
    /// `let`/`const`/`class` name in any enclosing scope up to the function
    /// boundary. Function-declared names coexist with `var`.
    pub(crate) fn declare_var(&mut self, name: AtomId, start: u32) -> Result<(), JsError> {
        for scope in self.scopes.iter().rev() {
            if scope.lexical.contains(&name) && !scope.functions.contains(&name) {
                return Err(self.error_at(start, "Identifier has already been declared"));
            }
            if scope.is_function {
                break;
            }
        }
        self.list_vars.insert(name);
        Ok(())
    }

    /// Declares a function-declaration name: it coexists with `var`, clashes
    /// with `let`/`const`, and may repeat only in sloppy mode.
    pub(crate) fn declare_function(&mut self, name: AtomId, start: u32) -> Result<(), JsError> {
        let scope = self.scopes.last_mut().unwrap();
        if scope.lexical.contains(&name) && !scope.functions.contains(&name) {
            return Err(self.error_at(start, "Identifier has already been declared"));
        }
        if self.strict && scope.functions.contains(&name) {
            return Err(self.error_at(start, "Identifier has already been declared"));
        }
        scope.lexical.insert(name);
        scope.functions.insert(name);
        self.list_vars.insert(name);
        Ok(())
    }

    /// Strict-mode restriction: `eval` and `arguments` cannot be bound.
    pub(crate) fn check_binding_name(&self, name: AtomId, start: u32) -> Result<(), JsError> {
        if self.strict && (name == intern_utf8("eval") || name == intern_utf8("arguments")) {
            return Err(self.error_at(start, "Unexpected eval or arguments in strict mode"));
        }
        Ok(())
    }

    /// Validates an assignment/update target: simple references and members,
    /// plus array/object patterns for plain `=` (spec 13.15 early errors).
    pub(crate) fn check_assignment_target(
        &mut self,
        expr: &Expr,
        op: AssignOp,
    ) -> Result<(), JsError> {
        self.check_target(expr, op == AssignOp::Assign)
    }

    fn check_target(&mut self, expr: &Expr, allow_pattern: bool) -> Result<(), JsError> {
        match &expr.kind {
            ExprKind::Ident(name) => {
                if self.strict
                    && (*name == intern_utf8("eval") || *name == intern_utf8("arguments"))
                {
                    return Err(self.error_at(
                        expr.span.start,
                        "Cannot assign to eval or arguments in strict mode",
                    ));
                }
                Ok(())
            }
            ExprKind::Paren(inner) => self.check_target(inner, allow_pattern),
            ExprKind::Member(_) => {
                if crate::expr::contains_optional(expr) {
                    return Err(self.error_at(
                        expr.span.start,
                        "Invalid optional chain as assignment target",
                    ));
                }
                Ok(())
            }
            ExprKind::Array(_) | ExprKind::Object(_) if allow_pattern => Ok(()),
            _ => Err(self.error_at(expr.span.start, "Invalid assignment target")),
        }
    }

    // ---- identifiers and binding patterns ----

    /// Parses an identifier that is legal to reference, applying the
    /// reserved-word rules for the current context.
    pub(crate) fn parse_identifier(&mut self) -> Result<(AtomId, u32), JsError> {
        let tok = self.peek()?.clone();
        let TokenKind::Identifier(atom) = tok.kind else {
            return Err(self.unexpected(&tok));
        };
        if from_identifier(atom).is_some() {
            return Err(self.error_at(tok.span.start, "Unexpected keyword"));
        }
        if self.strict && is_future_reserved_word(atom) {
            return Err(self.error_at(tok.span.start, "Unexpected reserved word"));
        }
        if self.in_generator && atom == intern_utf8("yield") {
            return Err(self.error_at(tok.span.start, "Unexpected yield"));
        }
        self.next()?;
        Ok((atom, tok.span.start))
    }

    /// Parses a binding pattern (identifier, array, or object) followed by
    /// an optional initializer.
    pub(crate) fn parse_binding_element(&mut self) -> Result<BindingElement, JsError> {
        let start = self.peek()?.span.start;
        let pattern = self.parse_binding_pattern()?;
        let init = if self.eat_punct(TokenKind::Equal)? {
            Some(crate::expr::parse_assignment(self, true)?)
        } else {
            None
        };
        let end = self.prev.as_ref().unwrap().span.end;
        Ok(BindingElement {
            pattern,
            init,
            span: Span::new(start, end),
        })
    }

    pub(crate) fn parse_binding_pattern(&mut self) -> Result<BindingPattern, JsError> {
        match self.peek()?.kind.clone() {
            TokenKind::Identifier(_) => {
                let (name, start) = self.parse_identifier()?;
                self.check_binding_name(name, start)?;
                Ok(BindingPattern::Ident(name))
            }
            TokenKind::LeftBracket => self.parse_array_pattern(),
            TokenKind::LeftBrace => self.parse_object_pattern(),
            _ => {
                let tok = self.peek()?.clone();
                Err(self.unexpected(&tok))
            }
        }
    }

    fn parse_array_pattern(&mut self) -> Result<BindingPattern, JsError> {
        self.next()?; // '['
        let mut elements: Vec<ArrayBindingElement> = Vec::new();
        loop {
            if self.eat_punct(TokenKind::RightBracket)? {
                break;
            }
            if self.at_punct(TokenKind::Comma)? {
                self.next()?;
                elements.push(ArrayBindingElement::Hole);
                continue;
            }
            if self.eat_punct(TokenKind::Ellipsis)? {
                // Rest must be the final element and takes no initializer.
                let element = self.parse_rest_binding()?;
                elements.push(ArrayBindingElement::Rest(element));
                self.expect_punct(TokenKind::RightBracket)?;
                break;
            }
            elements.push(ArrayBindingElement::Element(self.parse_binding_element()?));
            if !self.eat_punct(TokenKind::Comma)? {
                self.expect_punct(TokenKind::RightBracket)?;
                break;
            }
        }
        Ok(BindingPattern::Array(elements))
    }

    /// Parses `...pattern` (no initializer allowed).
    fn parse_rest_binding(&mut self) -> Result<BindingElement, JsError> {
        let start = self.peek()?.span.start;
        let pattern = self.parse_binding_pattern()?;
        if self.at_punct(TokenKind::Equal)? {
            return Err(self.error_at(start, "Rest parameter may not have a default initializer"));
        }
        let end = self.prev.as_ref().unwrap().span.end;
        Ok(BindingElement {
            pattern,
            init: None,
            span: Span::new(start, end),
        })
    }

    fn parse_object_pattern(&mut self) -> Result<BindingPattern, JsError> {
        self.next()?; // '{'
        let mut props: Vec<ObjectBindingProperty> = Vec::new();
        while !self.at_punct(TokenKind::RightBrace)? {
            let start = self.peek()?.span.start;
            if self.eat_punct(TokenKind::Ellipsis)? {
                let element = self.parse_rest_binding()?;
                props.push(ObjectBindingProperty::Rest(element));
                self.expect_punct(TokenKind::RightBrace)?;
                return Ok(BindingPattern::Object(props));
            }
            let key = self.parse_property_name()?;
            if self.eat_punct(TokenKind::Colon)? {
                let element = self.parse_binding_element()?;
                props.push(ObjectBindingProperty::Property {
                    key,
                    element,
                    span: Span::new(start, self.prev.as_ref().unwrap().span.end),
                });
            } else {
                // Shorthand: `{ x }` or `{ x = 1 }`; the key must be a plain
                // identifier.
                let PropertyName::Ident(name) = key else {
                    return Err(self.error_at(start, "Invalid shorthand property name"));
                };
                let init = if self.eat_punct(TokenKind::Equal)? {
                    Some(crate::expr::parse_assignment(self, true)?)
                } else {
                    None
                };
                let end = self.prev.as_ref().unwrap().span.end;
                props.push(ObjectBindingProperty::Property {
                    key: PropertyName::Ident(name),
                    element: BindingElement {
                        pattern: BindingPattern::Ident(name),
                        init,
                        span: Span::new(start, end),
                    },
                    span: Span::new(start, end),
                });
            }
            if !self.eat_punct(TokenKind::Comma)? {
                break;
            }
        }
        self.expect_punct(TokenKind::RightBrace)?;
        Ok(BindingPattern::Object(props))
    }

    /// Parses a property name: identifier, string, number, or computed.
    pub(crate) fn parse_property_name(&mut self) -> Result<PropertyName, JsError> {
        match self.peek()?.kind.clone() {
            TokenKind::Identifier(atom) => {
                self.next()?;
                Ok(PropertyName::Ident(atom))
            }
            TokenKind::StringLiteral { value, .. } => {
                self.next()?;
                Ok(PropertyName::Str(value))
            }
            TokenKind::NumericLiteral(value) => {
                self.next()?;
                Ok(PropertyName::Number(match value {
                    syntax::NumericLiteral::Number(n) => n,
                    syntax::NumericLiteral::BigInt(_) => {
                        return Err(self.error_at(
                            self.prev.as_ref().unwrap().span.start,
                            "Unexpected BigInt property name",
                        ));
                    }
                }))
            }
            TokenKind::LeftBracket => {
                self.next()?;
                let expr = crate::expr::parse_assignment(self, true)?;
                self.expect_punct(TokenKind::RightBracket)?;
                Ok(PropertyName::Computed(expr))
            }
            _ => {
                let tok = self.peek()?.clone();
                Err(self.unexpected(&tok))
            }
        }
    }
}
