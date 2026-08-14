//! The parser core: token helpers, ASI, context flags, and early-error
//! scope tracking (spec ch. 13-17).

use std::collections::HashSet;

use crux::{AtomId, JsError, Span, intern_utf8};
use syntax::keywords::{Keyword, from_identifier, is_future_reserved_word};
use syntax::{
    ArrayBindingElement, ArrayElement, AssignOp, BindingElement, BindingPattern, Expr, ExprKind,
    ObjectBindingProperty, ObjectProperty, PropertyName, Token, TokenKind,
};

use crate::token_stream::{TokenStream, can_end_expression};

/// How a private name is declared, for the getter/setter-pair rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PrivateNameKind {
    #[default]
    None,
    Getter(bool),
    Setter(bool),
    GetterSetter {
        getter_static: bool,
        setter_static: bool,
    },
    Other,
}

impl PrivateNameKind {
    pub(crate) fn with_static(self, is_static: bool) -> PrivateNameKind {
        match self {
            PrivateNameKind::Getter(_) => PrivateNameKind::Getter(is_static),
            PrivateNameKind::Setter(_) => PrivateNameKind::Setter(is_static),
            PrivateNameKind::Other => PrivateNameKind::Other,
            other => other,
        }
    }
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
    /// Catch-parameter names: a block-level function declaration may share a
    /// catch parameter's name (Annex B), though `let`/`const` may not.
    pub(crate) catch_params: HashSet<AtomId>,
    /// True for the scope of a function body: `var` names do not escape it.
    pub(crate) is_function: bool,
    /// True for the scope of a catch block: the catch parameter is a lexical
    /// binding there, but it does not conflict with `var` declarations in the
    /// catch body (the spec's var-rule was relaxed).
    pub(crate) is_catch: bool,
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
    /// Parsing a module: `await` is a reserved word, import/export only at
    /// the top level, no Annex B HTML comments.
    pub(crate) in_module: bool,
    /// Module top-level: `await` may be used as an expression.
    pub(crate) top_level_await: bool,
    /// Inside a class method/field/static-block body: `super.x` is legal.
    pub(crate) allow_super: bool,
    /// Inside a constructor: `super()` is legal.
    pub(crate) in_constructor: bool,
    /// Inside a class field initializer: `new.target` is legal there even
    /// outside functions (spec: field initializers are not contained by the
    /// enclosing StatementList).
    pub(crate) in_field_initializer: bool,
    /// Per-class private-name declarations, for duplicate checks.
    pub(crate) private_names: Vec<std::collections::HashMap<AtomId, PrivateNameKind>>,
    /// Inside arrow-function parameter cover grammar; `{a = 1}` shorthand
    /// initializers are only legal here (spec 13.2.5 CoverInitializedName).
    pub(crate) in_arrow_cover: bool,
    /// First CoverInitializedName parsed, if the enclosing construct never
    /// disambiguates into a pattern (an arrow list, an assignment target, or
    /// a for-in/of head).
    pub(crate) cover_error: Option<Span>,
    /// Depth of array/object literals and for-heads being parsed that may
    /// still become patterns: while positive, a pending `cover_error` is not
    /// raised by `parse_assignment` (the enclosing literal decides).
    pub(crate) suppress_cover_raise: usize,

    // Early-error tracking.
    pub(crate) scopes: Vec<Scope>,
    /// `var` names declared in the current statement list (per the
    /// lexical-vs-var disjointness rule).
    pub(crate) list_vars: HashSet<AtomId>,
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
            in_module: false,
            top_level_await: false,
            allow_super: false,
            in_constructor: false,
            in_field_initializer: false,
            private_names: Vec::new(),
            in_arrow_cover: false,
            cover_error: None,
            suppress_cover_raise: 0,
            scopes: vec![Scope::default()],
            list_vars: HashSet::new(),
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

    pub(crate) fn peek3(&mut self) -> Result<&Token, JsError> {
        self.stream.peek3()
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

    /// Consumes the current token if it is the contextual keyword `text`.
    pub(crate) fn eat_contextual(&mut self, text: &str) -> Result<bool, JsError> {
        if self.at_contextual(text)? {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Requires the current token to be the contextual keyword `text`.
    pub(crate) fn expect_contextual(&mut self, text: &str) -> Result<(), JsError> {
        if self.eat_contextual(text)? {
            Ok(())
        } else {
            let tok = self.peek()?.clone();
            Err(self.unexpected(&tok))
        }
    }

    /// Statement-parser entry point for contexts outside `stmt` (modules).
    pub(crate) fn parse_statement_public(&mut self) -> Result<syntax::Stmt, JsError> {
        crate::stmt::parse_statement(self, true)
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
        if (self.in_async || self.in_module) && atom == intern_utf8("await") {
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

    /// Declares a `let`/`const`/`class`/`using` name: duplicates in the same
    /// scope, clashes with `var`/function names in the same statement list,
    /// and clashes with a formal parameter are early errors.
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

    /// Declares a catch-parameter name: duplicates inside the pattern are
    /// early errors, but the parameter lives in its own scope and does not
    /// clash with `var` names in the enclosing statement list (spec 15.1.8).
    pub(crate) fn declare_catch_param(&mut self, name: AtomId, start: u32) -> Result<(), JsError> {
        let scope = self.scopes.last_mut().unwrap();
        if scope.lexical.contains(&name) || scope.params.contains(&name) {
            return Err(self.error_at(start, "Identifier has already been declared"));
        }
        scope.lexical.insert(name);
        scope.catch_params.insert(name);
        Ok(())
    }

    /// Declares a `var` name: it may repeat, but must not clash with a
    /// `let`/`const`/`class` name in any enclosing scope up to the function
    /// boundary. Function-declared names coexist with `var`; catch-parameter
    /// names are skipped (a `var` in the catch body may share the name).
    pub(crate) fn declare_var(&mut self, name: AtomId, start: u32) -> Result<(), JsError> {
        for scope in self.scopes.iter().rev() {
            if scope.is_catch {
                continue;
            }
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
    /// with `let`/`const`, and may repeat (function declarations are
    /// var-scoped in both modes, spec 16.1.2). A block-level function may
    /// share a catch parameter's name (Annex B); a statement-position
    /// function (`if (x) function f(){}`) may share an enclosing lexical
    /// name (its hoist is simply suppressed).
    pub(crate) fn declare_function(
        &mut self,
        name: AtomId,
        start: u32,
        relaxed: bool,
    ) -> Result<(), JsError> {
        let scope = self.scopes.last_mut().unwrap();
        let is_catch_param = scope.is_catch && scope.catch_params.contains(&name);
        if scope.lexical.contains(&name)
            && !scope.functions.contains(&name)
            && !is_catch_param
            && !relaxed
        {
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
            ExprKind::Array(_) | ExprKind::Object(_) if allow_pattern => {
                self.check_rest_position(expr)
            }
            // Annex B web-compat: a CallExpression assignment target parses in
            // sloppy code and throws a ReferenceError at runtime (spec
            // "Runtime Errors for Function Call Assignment Targets"); strict
            // code keeps the early error.
            ExprKind::Call(_) if !self.strict => Ok(()),
            _ => Err(self.error_at(expr.span.start, "Invalid assignment target")),
        }
    }

    /// The assignment-pattern rest rule (spec 13.15.1/13.2.2): in an array or
    /// object assignment target, no element may follow a spread element. This
    /// only inspects the target's own structure; default-value expressions
    /// inside are not patterns.
    fn check_rest_position(&mut self, expr: &Expr) -> Result<(), JsError> {
        let mut seen_rest = false;
        match &expr.kind {
            ExprKind::Array(lit) => {
                if lit.rest_trailing_comma {
                    return Err(self.error_at(lit.span.start, "Rest element must be last"));
                }
                for el in &lit.elements {
                    match el {
                        ArrayElement::Hole => {
                            if seen_rest {
                                return Err(
                                    self.error_at(expr.span.start, "Rest element must be last")
                                );
                            }
                        }
                        ArrayElement::Expr(e) => {
                            if seen_rest {
                                return Err(
                                    self.error_at(expr.span.start, "Rest element must be last")
                                );
                            }
                            self.check_element_target(e)?;
                        }
                        ArrayElement::Spread(e) => {
                            if seen_rest {
                                return Err(
                                    self.error_at(e.span.start, "Rest element must be last")
                                );
                            }
                            // A rest target may not carry an initializer
                            // (spec 13.15.5.1 AssignmentRestElement).
                            if matches!(e.kind, ExprKind::Assign { .. }) {
                                return Err(self.error_at(e.span.start, "Invalid rest element"));
                            }
                            self.check_element_target(e)?;
                            seen_rest = true;
                        }
                    }
                }
            }
            ExprKind::Object(lit) => {
                if lit.rest_trailing_comma {
                    return Err(self.error_at(lit.span.start, "Rest element must be last"));
                }
                for prop in &lit.props {
                    match prop {
                        ObjectProperty::Init { value, .. } => {
                            if seen_rest {
                                return Err(
                                    self.error_at(value.span.start, "Rest element must be last")
                                );
                            }
                            self.check_element_target(value)?;
                        }
                        ObjectProperty::Spread(e) => {
                            if seen_rest {
                                return Err(
                                    self.error_at(e.span.start, "Rest element must be last")
                                );
                            }
                            self.check_element_target(e)?;
                            seen_rest = true;
                        }
                        // Methods, accessors, and fields are never valid in an
                        // assignment pattern (spec 13.15.5.1).
                        _ => {
                            return Err(self.error_at(expr.span.start, "Invalid assignment target"));
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Validates one element/property target of an assignment pattern: a
    /// plain `=` unwraps to its target, which must be a reference, a member
    /// expression, or a nested pattern (spec 13.15.5.1).
    fn check_element_target(&mut self, expr: &Expr) -> Result<(), JsError> {
        let target = match &expr.kind {
            ExprKind::Assign {
                op: AssignOp::Assign,
                target,
                ..
            } => target.as_ref(),
            _ => expr,
        };
        match &target.kind {
            ExprKind::Ident(name) => {
                if self.strict
                    && (*name == intern_utf8("eval") || *name == intern_utf8("arguments"))
                {
                    return Err(self.error_at(
                        target.span.start,
                        "Cannot assign to eval or arguments in strict mode",
                    ));
                }
                Ok(())
            }
            ExprKind::Paren(inner) => self.check_element_target(inner),
            ExprKind::Member(_) => {
                if crate::expr::contains_optional(target) {
                    return Err(self.error_at(
                        target.span.start,
                        "Invalid optional chain as assignment target",
                    ));
                }
                Ok(())
            }
            ExprKind::Array(_) | ExprKind::Object(_) => self.check_rest_position(target),
            _ => Err(self.error_at(target.span.start, "Invalid assignment target")),
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
        if (self.in_async || self.in_module) && atom == intern_utf8("await") {
            return Err(self.error_at(tok.span.start, "Unexpected await"));
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
            rest: false,
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
            rest: false,
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
                // identifier, and a BindingIdentifier may not be a reserved
                // word or (in strict mode) `eval`/`arguments` (spec 13.1.1).
                let PropertyName::Ident(name) = key else {
                    return Err(self.error_at(start, "Invalid shorthand property name"));
                };
                if from_identifier(name).is_some()
                    || (self.strict && is_future_reserved_word(name))
                    || (self.strict
                        && (name == intern_utf8("eval") || name == intern_utf8("arguments")))
                    || (self.in_generator && name == intern_utf8("yield"))
                    || ((self.in_async || self.in_module) && name == intern_utf8("await"))
                {
                    return Err(self.error_at(start, "Unexpected reserved word"));
                }
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
                        rest: false,
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
