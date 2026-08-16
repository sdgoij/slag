//! Statement and declaration parsing (spec ch. 14).

use crux::{AtomId, JsError, Span, intern_utf8};
use syntax::keywords::{Keyword, from_identifier};
use syntax::{
    BindingPattern, Block, CatchClause, ForBinding, ForInit, Stmt, StmtKind, SwitchCase, Token,
    TokenKind, VarDeclKind, VarDeclarator,
};

use crate::expr::{can_start_expression, parse_assignment, parse_expression};
use crate::parser::Parser;

/// Parses statements until the terminator token or EOF. `var` names
/// accumulate in `parser.list_vars`; the caller decides whether they
/// propagate past the statement-list boundary (blocks propagate, function
/// bodies do not).
pub(crate) fn parse_statement_list(
    parser: &mut Parser,
    terminator: TokenKind,
) -> Result<Vec<Stmt>, JsError> {
    let mut stmts = Vec::new();
    while !matches!(parser.peek()?.kind, TokenKind::Eof) && !parser.at_punct(terminator.clone())? {
        stmts.push(parse_statement(parser, true)?);
    }
    Ok(stmts)
}

pub(crate) fn parse_statement(
    parser: &mut Parser,
    allow_declaration: bool,
) -> Result<Stmt, JsError> {
    parse_statement_with(parser, allow_declaration, false)
}

/// Like `parse_statement`, but the position allows the Annex B web-compat
/// plain-function relaxations: a `function` declaration as the body of an
/// `if` clause (B.3.4) or of a labelled statement (B.3.2), sloppy mode only.
pub(crate) fn parse_statement_with(
    parser: &mut Parser,
    allow_declaration: bool,
    annex_b_function: bool,
) -> Result<Stmt, JsError> {
    let stmt = parse_statement_inner(parser, allow_declaration, annex_b_function)?;
    // A statement ending in `}` (block, function/class declaration, switch,
    // or a body block) is followed by an expression start: the next `/` must
    // lex as a RegularExpressionLiteral, not division (spec 12.1).
    if matches!(
        parser.prev.as_ref().map(|t| &t.kind),
        Some(TokenKind::RightBrace)
    ) {
        parser.after_statement_brace = true;
    }
    Ok(stmt)
}

fn parse_statement_inner(
    parser: &mut Parser,
    allow_declaration: bool,
    annex_b_function: bool,
) -> Result<Stmt, JsError> {
    let start = parser.peek()?.span.start;
    let tok = parser.peek()?.clone();
    let kind = match tok.kind.clone() {
        TokenKind::LeftBrace => {
            return parse_block(parser).map(|b| Stmt {
                span: b.span,
                kind: StmtKind::Block(b),
            });
        }
        TokenKind::Semicolon => {
            parser.next()?;
            StmtKind::Empty
        }
        TokenKind::At => {
            // A decorated class declaration: `@dec class C { … }`. The
            // decorators are validated syntactically and not evaluated.
            if !allow_declaration {
                return Err(parser.error_at(
                    start,
                    "Lexical declaration cannot appear in a single-statement context",
                ));
            }
            crate::class::parse_decorators(parser)?;
            parser.expect_keyword(Keyword::Class)?;
            let class_start = parser.prev.as_ref().unwrap().span.start;
            let class = crate::class::parse_class(parser, class_start, true)?;
            let end = class.span.end;
            return Ok(Stmt {
                span: Span::new(start, end),
                kind: StmtKind::ClassDecl(class),
            });
        }
        TokenKind::Identifier(atom) => match from_identifier(atom) {
            Some(Keyword::Var) => return parse_var_statement(parser, VarDeclKind::Var),
            Some(Keyword::Const) => {
                // LexicalDeclaration is a Declaration, not a Statement; it is
                // only allowed at StatementListItem level.
                if !allow_declaration {
                    return Err(parser.error_at(
                        start,
                        "Lexical declaration cannot appear in a single-statement context",
                    ));
                }
                return parse_var_statement(parser, VarDeclKind::Const);
            }
            Some(Keyword::If) => return parse_if(parser),
            Some(Keyword::Do) => return parse_do_while(parser),
            Some(Keyword::While) => return parse_while(parser),
            Some(Keyword::For) => return parse_for(parser),
            Some(Keyword::Continue) => return parse_continue(parser),
            Some(Keyword::Break) => return parse_break(parser),
            Some(Keyword::Return) => return parse_return(parser),
            Some(Keyword::With) => return parse_with(parser),
            Some(Keyword::Switch) => return parse_switch(parser),
            Some(Keyword::Throw) => return parse_throw(parser),
            Some(Keyword::Try) => return parse_try(parser),
            Some(Keyword::Debugger) => {
                parser.next()?;
                parser.expect_semicolon()?;
                StmtKind::Debugger
            }
            Some(Keyword::Function) => {
                return parse_function_declaration(
                    parser,
                    false,
                    !allow_declaration,
                    annex_b_function,
                );
            }
            Some(Keyword::Class) => {
                if !allow_declaration {
                    return Err(parser.error_at(
                        start,
                        "Lexical declaration cannot appear in a single-statement context",
                    ));
                }
                let start = parser.next()?.span.start;
                let class = crate::class::parse_class(parser, start, true)?;
                let end = class.span.end;
                return Ok(Stmt {
                    span: Span::new(start, end),
                    kind: StmtKind::ClassDecl(class),
                });
            }
            _ if atom == intern_utf8("let")
                && !tok.escaped
                && is_let_declaration_start(parser.peek2()?.kind.clone()) =>
            {
                if !allow_declaration {
                    // `let [` is excluded from ExpressionStatement even
                    // across a line break (spec 13.8), so `if (x) let\n[a]`
                    // is an early error rather than an ASI'd `let` reference.
                    if matches!(parser.peek2()?.kind, TokenKind::LeftBracket) {
                        return Err(parser.error_at(start, "Unexpected token '[' after let"));
                    }
                    if !parser.peek2()?.line_break_before {
                        return Err(parser.error_at(
                            start,
                            "Lexical declaration cannot appear in a single-statement context",
                        ));
                    }
                    // Statement position with a line break before the
                    // declaration start: `let` is an expression statement
                    // (ASI), e.g. `if (x) let\n{}`.
                    if is_label_start(parser)? {
                        return parse_labeled(parser);
                    }
                    return parse_expression_statement(parser);
                }
                return parse_var_statement(parser, VarDeclKind::Let);
            }
            _ if atom == intern_utf8("using")
                && !tok.escaped
                && is_using_binding_start(parser.peek2()?.kind.clone())
                && !parser.peek2()?.line_break_before =>
            {
                if !allow_declaration {
                    return Err(parser.error_at(
                        start,
                        "Lexical declaration cannot appear in a single-statement context",
                    ));
                }
                return parse_using_declaration(parser, false);
            }
            _ if atom == intern_utf8("await")
                && !tok.escaped
                && (parser.in_async || parser.top_level_await)
                && parser.peek2()?.kind == TokenKind::Identifier(intern_utf8("using"))
                && !parser.peek2()?.line_break_before
                && is_using_binding_start(parser.peek3()?.kind.clone())
                && !parser.peek3()?.line_break_before =>
            {
                if !allow_declaration {
                    return Err(parser.error_at(
                        start,
                        "Lexical declaration cannot appear in a single-statement context",
                    ));
                }
                parser.next()?; // `await`
                return parse_using_declaration(parser, true);
            }
            _ if atom == intern_utf8("async")
                && !tok.escaped
                && parser.peek2()?.kind == TokenKind::Identifier(intern_utf8("function"))
                && !parser.peek2()?.line_break_before =>
            {
                parser.next()?; // `async`
                return parse_function_declaration(
                    parser,
                    true,
                    !allow_declaration,
                    annex_b_function,
                );
            }
            _ => {
                if is_label_start(parser)? {
                    return parse_labeled(parser);
                }
                return parse_expression_statement(parser);
            }
        },
        _ => return parse_expression_statement(parser),
    };
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind,
    })
}

/// `let` starts a declaration when followed by a binding identifier, `[`, or
/// `{`. Keywords and the `of` contextual keyword after `let` mean `let` is an
/// identifier (the `[lookahead ∉ { let [, let of }]` restrictions of the
/// for-head grammar).
fn is_let_declaration_start(kind: TokenKind) -> bool {
    match kind {
        TokenKind::Identifier(atom) => from_identifier(atom).is_none(),
        TokenKind::LeftBracket | TokenKind::LeftBrace => true,
        _ => false,
    }
}

/// A `using` token starts a declaration when followed by a non-keyword
/// identifier on the same line. The BindingList of a `using` declaration is
/// identifier-only (`~Pattern`), so `[`, `{`, and keywords mean the `using`
/// token is an identifier reference (spec 15.14.1).
fn is_using_binding_start(kind: TokenKind) -> bool {
    match kind {
        TokenKind::Identifier(atom) => from_identifier(atom).is_none(),
        _ => false,
    }
}

/// `ident :` begins a labeled statement (no line break before the colon).
fn is_label_start(parser: &mut Parser) -> Result<bool, JsError> {
    let tok = parser.peek()?.clone();
    let TokenKind::Identifier(atom) = tok.kind else {
        return Ok(false);
    };
    if from_identifier(atom).is_some() {
        return Ok(false);
    }
    let next = parser.peek2()?.clone();
    Ok(next.kind == TokenKind::Colon && !next.line_break_before)
}

fn parse_labeled(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.peek()?.span.start;
    let (name, _) = parser.parse_identifier()?;
    if name == intern_utf8("yield") && parser.in_generator {
        return Err(parser.error_at(start, "Unexpected yield label"));
    }
    if name == intern_utf8("await") && parser.in_async {
        return Err(parser.error_at(start, "Unexpected await label"));
    }
    if name == intern_utf8("let") {
        return Err(parser.error_at(start, "Unexpected strict mode reserved word"));
    }
    parser.expect_punct(TokenKind::Colon)?;
    let body = Box::new(parse_statement_with(parser, false, true)?);
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::Labeled { label: name, body },
    })
}

/// IsLabelledFunction (spec 13.13): a labelled statement whose innermost
/// statement is a FunctionDeclaration. Such a declaration is never permitted
/// as the sole body of an if/while/do/for/with statement.
fn is_labelled_function(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Labeled { body, .. } => match &body.kind {
            StmtKind::FunctionDecl(_) => true,
            StmtKind::Labeled { .. } => is_labelled_function(body),
            _ => false,
        },
        _ => false,
    }
}

fn parse_block(parser: &mut Parser) -> Result<Block, JsError> {
    let start = parser.next()?.span.start; // '{'
    let saved_vars = std::mem::take(&mut parser.list_vars);
    parser.push_scope();
    parser.scopes.last_mut().unwrap().is_block = true;
    let stmts = parse_statement_list(parser, TokenKind::RightBrace)?;
    parser.expect_punct(TokenKind::RightBrace)?;
    let end = parser.prev.as_ref().unwrap().span.end;
    parser.pop_scope();
    // `var` names declared inside the block propagate to the enclosing
    // statement list (VarDeclaredNames of a block is its statement list's).
    parser.list_vars.extend(saved_vars);
    Ok(Block {
        stmts,
        span: Span::new(start, end),
    })
}

fn parse_expression_statement(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.peek()?.span.start;
    let expr = parse_expression(parser, true)?;
    parser.expect_semicolon()?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::Expr(expr),
    })
}

fn parse_var_statement(parser: &mut Parser, kind: VarDeclKind) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // var/let/const
    let decls = parse_var_declarators(parser, kind)?;
    parser.expect_semicolon()?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::VarDecl { kind, decls },
    })
}

/// `using x = expr, …` / `await using x = expr, …` (spec 15.14). The `using`
/// token is already consumed; bindings are identifiers with required
/// initializers, declared lexically.
fn parse_using_declaration(parser: &mut Parser, is_await: bool) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `using`
    // A UsingDeclaration is only valid inside a function body, a module, or
    // a block — not directly in a script's top-level statement list (spec
    // 15.14.2), since there is no enclosing resource-management scope.
    if parser.scopes.len() == 1 && !parser.in_module {
        return Err(parser.error_at(
            start,
            "using declarations are not allowed at the top level of a script",
        ));
    }
    let mut decls = Vec::new();
    loop {
        let decl_start = parser.peek()?.span.start;
        let (name, name_start) = parser.parse_identifier()?;
        // `let` is never a valid bound name of a using declaration (spec
        // 15.14.2 early errors).
        if name == intern_utf8("let") {
            return Err(parser.error_at(name_start, "let is disallowed as a lexically bound name"));
        }
        parser.check_binding_name(name, name_start)?;
        parser.declare_lexical(name, name_start)?;
        if !parser.eat_punct(TokenKind::Equal)? {
            return Err(parser.error_at(decl_start, "Missing initializer in using declaration"));
        }
        let init = parse_assignment(parser, true)?;
        let end = parser.prev.as_ref().unwrap().span.end;
        decls.push(VarDeclarator {
            pattern: BindingPattern::Ident(name),
            init: Some(init),
            span: Span::new(decl_start, end),
        });
        if !parser.eat_punct(TokenKind::Comma)? {
            break;
        }
    }
    parser.expect_semicolon()?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::UsingDecl { is_await, decls },
    })
}

/// Parses the comma-separated declarators of a variable declaration.
pub(crate) fn parse_var_declarators(
    parser: &mut Parser,
    kind: VarDeclKind,
) -> Result<Vec<VarDeclarator>, JsError> {
    let mut decls = Vec::new();
    loop {
        let start = parser.peek()?.span.start;
        let pattern = parser.parse_binding_pattern()?;
        for name in bound_names(&pattern) {
            parser.check_binding_name(name, start)?;
            match kind {
                VarDeclKind::Var => parser.declare_var(name, start)?,
                VarDeclKind::Let | VarDeclKind::Const => {
                    // `let` is never a valid bound name of a lexical
                    // declaration (spec 14.2.1 early errors).
                    if name == intern_utf8("let") {
                        return Err(
                            parser.error_at(start, "let is disallowed as a lexically bound name")
                        );
                    }
                    parser.declare_lexical(name, start)?;
                }
                VarDeclKind::Using | VarDeclKind::AwaitUsing => {
                    parser.declare_lexical(name, start)?
                }
            }
        }
        let init = if parser.eat_punct(TokenKind::Equal)? {
            Some(parse_assignment(parser, true)?)
        } else {
            None
        };
        if kind == VarDeclKind::Const && init.is_none() {
            return Err(parser.error_at(start, "Missing initializer in const declaration"));
        }
        let end = parser.prev.as_ref().unwrap().span.end;
        decls.push(VarDeclarator {
            pattern,
            init,
            span: Span::new(start, end),
        });
        if !parser.eat_punct(TokenKind::Comma)? {
            break;
        }
    }
    Ok(decls)
}

/// Collects the identifier names bound by a pattern.
pub(crate) fn bound_names(pattern: &syntax::BindingPattern) -> Vec<AtomId> {
    let mut out = Vec::new();
    collect_bound_names(pattern, &mut out);
    out
}

/// Collects the names bound by a list of binding elements (parameters).
pub(crate) fn bound_names_of_elements(elements: &[syntax::BindingElement]) -> Vec<AtomId> {
    let mut out = Vec::new();
    for element in elements {
        collect_bound_names(&element.pattern, &mut out);
    }
    out
}

fn collect_bound_names(pattern: &syntax::BindingPattern, out: &mut Vec<AtomId>) {
    match pattern {
        syntax::BindingPattern::Ident(name) => out.push(*name),
        syntax::BindingPattern::Array(elements) => {
            for el in elements {
                match el {
                    syntax::ArrayBindingElement::Hole => {}
                    syntax::ArrayBindingElement::Element(e)
                    | syntax::ArrayBindingElement::Rest(e) => {
                        collect_bound_names(&e.pattern, out);
                    }
                }
            }
        }
        syntax::BindingPattern::Object(props) => {
            for prop in props {
                match prop {
                    syntax::ObjectBindingProperty::Property { element, .. } => {
                        collect_bound_names(&element.pattern, out);
                    }
                    syntax::ObjectBindingProperty::Rest(e) => {
                        collect_bound_names(&e.pattern, out);
                    }
                }
            }
        }
    }
}

fn parse_if(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `if`
    parser.expect_punct(TokenKind::LeftParen)?;
    let test = parse_expression(parser, true)?;
    parser.expect_punct(TokenKind::RightParen)?;
    let consequent = Box::new(parse_statement_with(parser, false, true)?);
    if is_labelled_function(&consequent) {
        return Err(parser.error_at(
            consequent.span.start,
            "A labelled function declaration is not allowed in statement position",
        ));
    }
    let alternate = if parser.eat_keyword(Keyword::Else)? {
        let stmt = parse_statement_with(parser, false, true)?;
        if is_labelled_function(&stmt) {
            return Err(parser.error_at(
                stmt.span.start,
                "A labelled function declaration is not allowed in statement position",
            ));
        }
        Some(Box::new(stmt))
    } else {
        None
    };
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::If {
            test,
            consequent,
            alternate,
        },
    })
}

fn parse_while(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `while`
    parser.expect_punct(TokenKind::LeftParen)?;
    let test = parse_expression(parser, true)?;
    parser.expect_punct(TokenKind::RightParen)?;
    let body = Box::new(parse_statement_with(parser, false, false)?);
    if is_labelled_function(&body) {
        return Err(parser.error_at(
            body.span.start,
            "A labelled function declaration is not allowed in statement position",
        ));
    }
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::While { test, body },
    })
}

fn parse_do_while(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `do`
    let body = Box::new(parse_statement_with(parser, false, false)?);
    if is_labelled_function(&body) {
        return Err(parser.error_at(
            body.span.start,
            "A labelled function declaration is not allowed in statement position",
        ));
    }
    parser.expect_keyword(Keyword::While)?;
    parser.expect_punct(TokenKind::LeftParen)?;
    let test = parse_expression(parser, true)?;
    parser.expect_punct(TokenKind::RightParen)?;
    // spec 12.10.1: the terminating semicolon of a do-while is inserted
    // before the next token even without a line terminator when the previous
    // token is `)` (the while clause's closing paren).
    parser.eat_punct(TokenKind::Semicolon)?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::DoWhile { body, test },
    })
}

fn parse_for(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `for`
    // `for await ( … of … )` — only in async functions and at module top
    // level (top-level await).
    let mut is_await = false;
    if parser.at_contextual("await")? && parser.peek2()?.kind == TokenKind::LeftParen {
        if !parser.in_async && !parser.top_level_await {
            let at = parser.peek()?.span.start;
            return Err(parser.error_at(at, "for await is only allowed in async functions"));
        }
        parser.next()?; // `await`
        is_await = true;
    }
    parser.expect_punct(TokenKind::LeftParen)?;

    // A lexical (let/const/using) head declares in the loop's own scope: the
    // names neither clash with the enclosing statement list nor contribute to
    // it (spec ForIn/ForOfStatement LexicallyDeclaredNames excludes the head;
    // a classic head is loop-scoped too, so it cannot clash with a sibling
    // loop's head or with a later `let` in the same list).
    let (head_kind, init_empty) = if parser.eat_punct(TokenKind::Semicolon)? {
        (None, true)
    } else if parser.at_keyword(Keyword::Var)? {
        let kind = VarDeclKind::Var;
        parser.next()?;
        (Some(kind), false)
    } else if parser.at_keyword(Keyword::Const)? {
        let kind = VarDeclKind::Const;
        parser.next()?;
        (Some(kind), false)
    } else if parser.at_contextual("let")? && is_let_declaration_start(parser.peek2()?.kind.clone())
    {
        let kind = VarDeclKind::Let;
        parser.next()?;
        (Some(kind), false)
    } else if parser.at_contextual("using")?
        && is_using_binding_start(parser.peek2()?.kind.clone())
        && !parser.peek2()?.line_break_before
        && !(parser.peek2()?.kind == TokenKind::Identifier(intern_utf8("of"))
            && parser.peek3()?.kind != TokenKind::Equal)
    {
        // `for (using x of y)` / `for (using x = 0; …)`. The `of` lookahead
        // only applies to for-of heads: `for (using of y)` is an
        // expression-headed for-of, while `for (using of = null;;)` is a
        // classic for with a binding named `of` (spec 14.7.5).
        let kind = VarDeclKind::Using;
        parser.next()?;
        (Some(kind), false)
    } else if parser.at_contextual("await")?
        && (parser.in_async || parser.top_level_await)
        && parser.peek2()?.kind == TokenKind::Identifier(intern_utf8("using"))
        && !parser.peek2()?.line_break_before
        && is_using_binding_start(parser.peek3()?.kind.clone())
        && !parser.peek3()?.line_break_before
    {
        // `for (await using x of y)` — an await-using ForDeclaration.
        let kind = VarDeclKind::AwaitUsing;
        parser.next()?; // `await`
        parser.next()?; // `using`
        (Some(kind), false)
    } else {
        (None, false)
    };
    let lexical = matches!(head_kind, Some(k) if k != VarDeclKind::Var);
    if lexical {
        parser.push_scope();
    }
    let saved_vars = if lexical {
        Some(std::mem::take(&mut parser.list_vars))
    } else {
        None
    };

    let init = match head_kind {
        Some(kind) => (
            Some(ForInit::VarDecl {
                kind,
                decls: parse_for_declarators(parser, kind)?,
            }),
            false,
        ),
        None if init_empty => (None, false),
        None => {
            // The expression-headed for-of production has the lookahead
            // restriction `[lookahead ∉ { let, async of }]` (spec 14.7.5):
            // an unescaped `async` immediately followed by `of` cannot be
            // the LHS of a non-await for-of. The for-await form has no such
            // restriction (`for await (async of …)` stays valid), an escaped
            // `async` is an ordinary identifier (spec 5.1.5.1), and
            // `async of => …` is an async-arrow init, not a for-of head.
            let mut async_of_head = !is_await
                && parser.at_contextual_unescaped("async")?
                && parser.peek2()?.kind == TokenKind::Identifier(intern_utf8("of"))
                && !parser.peek2()?.escaped;
            // The head may be a for-in/of pattern, so a cover form inside it is
            // deferred until the `in`/`of`/`;` decision is known.
            parser.suppress_cover_raise += 1;
            let init = parse_expression(parser, false)?;
            parser.suppress_cover_raise -= 1;
            // An async arrow init (`async of => …`) has already consumed the
            // `of`; the loop is then a classic for, so the restriction does
            // not apply.
            if matches!(init.kind, syntax::ExprKind::Arrow { .. }) {
                async_of_head = false;
            }
            (Some(ForInit::Expr(init)), async_of_head)
        }
    };
    let (init, async_of_head) = init;

    let stmt = if parser.at_keyword(Keyword::In)? {
        // `for await ( … in … )` and `using` heads have no for-in form.
        if is_await {
            return Err(parser.error_at(start, "for await is only valid with of"));
        }
        if matches!(
            &init,
            Some(ForInit::VarDecl { kind, .. })
                if matches!(*kind, VarDeclKind::Using | VarDeclKind::AwaitUsing)
        ) {
            return Err(parser.error_at(start, "using declarations are not allowed in for-in"));
        }
        parser.next()?;
        parser.cover_error = None;
        let left = for_binding_from_init(parser, init, true)?;
        let right = parse_expression(parser, true)?;
        parser.expect_punct(TokenKind::RightParen)?;
        let body = Box::new(parse_statement_with(parser, false, false)?);
        if is_labelled_function(&body) {
            return Err(parser.error_at(
                body.span.start,
                "A labelled function declaration is not allowed in statement position",
            ));
        }
        let end = parser.prev.as_ref().unwrap().span.end;
        Stmt {
            span: Span::new(start, end),
            kind: StmtKind::ForIn { left, right, body },
        }
    } else if parser.at_contextual("of")? {
        if is_await && parser.peek()?.line_break_before {
            return Err(parser.error_at(start, "Unexpected line break after for await"));
        }
        // spec 14.7.5: `for (async of …)` is a SyntaxError — the lookahead
        // `[lookahead ∉ { let, async of }]` rejects an unescaped `async`
        // immediately followed by `of` as the LHS.
        if async_of_head {
            return Err(parser.error_at(
                start,
                "async cannot be the left-hand side of a for-of statement",
            ));
        }
        parser.next()?;
        parser.cover_error = None;
        let left = for_binding_from_init(parser, init, false)?;
        let right = parse_assignment(parser, true)?;
        parser.expect_punct(TokenKind::RightParen)?;
        let body = Box::new(parse_statement_with(parser, false, false)?);
        if is_labelled_function(&body) {
            return Err(parser.error_at(
                body.span.start,
                "A labelled function declaration is not allowed in statement position",
            ));
        }
        let end = parser.prev.as_ref().unwrap().span.end;
        Stmt {
            span: Span::new(start, end),
            kind: StmtKind::ForOf {
                left,
                right,
                body,
                is_await,
            },
        }
    } else {
        // Classic `for ( init ; test ; update )`. When the init slot was empty,
        // its `;` is already consumed. A deferred cover form in the head is an
        // error here (the head is an expression, not a pattern).
        if let Some(span) = parser.cover_error.take() {
            return Err(parser.error_at(span.start, "Invalid shorthand property initializer"));
        }
        let test = if init_empty {
            if parser.at_punct(TokenKind::Semicolon)? {
                None
            } else {
                Some(parse_expression(parser, true)?)
            }
        } else {
            parser.expect_punct(TokenKind::Semicolon)?;
            // `const`/`using` declarators need initializers in a classic head
            // (for-in/of heads forbid them instead).
            if let Some(ForInit::VarDecl { kind, decls }) = &init {
                let needs_init = matches!(
                    kind,
                    VarDeclKind::Const | VarDeclKind::Using | VarDeclKind::AwaitUsing
                );
                if needs_init {
                    for decl in decls {
                        if decl.init.is_none() {
                            let message = match kind {
                                VarDeclKind::Const => "Missing initializer in const declaration",
                                _ => "Missing initializer in using declaration",
                            };
                            return Err(parser.error_at(decl.span.start, message));
                        }
                    }
                }
            }
            if parser.at_punct(TokenKind::Semicolon)? {
                None
            } else {
                Some(parse_expression(parser, true)?)
            }
        };
        parser.expect_punct(TokenKind::Semicolon)?;
        let update = if parser.at_punct(TokenKind::RightParen)? {
            None
        } else {
            Some(parse_expression(parser, true)?)
        };
        parser.expect_punct(TokenKind::RightParen)?;
        let body = Box::new(parse_statement_with(parser, false, false)?);
        if is_labelled_function(&body) {
            return Err(parser.error_at(
                body.span.start,
                "A labelled function declaration is not allowed in statement position",
            ));
        }
        let end = parser.prev.as_ref().unwrap().span.end;
        Stmt {
            span: Span::new(start, end),
            kind: StmtKind::For {
                init,
                test,
                update,
                body,
            },
        }
    };

    // Loop-scope restore: merge body/head `var` names back into the enclosing
    // statement list and drop the head scope. The head's lexical names stay in
    // the loop scope (a classic head does not declare into the enclosing list:
    // sibling `for (let i …)` loops and a later `let i` are both legal).
    if let Some(saved) = saved_vars {
        parser.list_vars.extend(saved);
    }
    if lexical {
        parser.pop_scope();
    }
    Ok(stmt)
}

/// Declarators in a for-head: `[~In]` throughout (the `in` of a for-in head
/// must not be consumed by an initializer), and exactly one binding when the
/// head turns out to be for-in/for-of.
fn parse_for_declarators(
    parser: &mut Parser,
    kind: VarDeclKind,
) -> Result<Vec<VarDeclarator>, JsError> {
    let mut decls = Vec::new();
    loop {
        let start = parser.peek()?.span.start;
        // A `using` ForBinding is identifier-only (`~Pattern`).
        if matches!(kind, VarDeclKind::Using | VarDeclKind::AwaitUsing)
            && !matches!(parser.peek()?.kind, TokenKind::Identifier(_))
        {
            let tok = parser.peek()?.clone();
            return Err(parser.unexpected(&tok));
        }
        let pattern = parser.parse_binding_pattern()?;
        for name in bound_names(&pattern) {
            parser.check_binding_name(name, start)?;
            match kind {
                VarDeclKind::Var => parser.declare_var(name, start)?,
                VarDeclKind::Let | VarDeclKind::Const => parser.declare_lexical(name, start)?,
                VarDeclKind::Using | VarDeclKind::AwaitUsing => {
                    parser.declare_lexical(name, start)?
                }
            }
            // `let` is never a valid bound name of a ForDeclaration (spec
            // 14.7.5 early errors); `var` heads are unrestricted.
            if kind != VarDeclKind::Var && name == intern_utf8("let") {
                return Err(parser.error_at(start, "let is disallowed as a lexically bound name"));
            }
        }
        // The initializer is `[~In]`: a `for (var x = a in b)` head must not
        // consume the `in` as part of the expression.
        let init = if parser.eat_punct(TokenKind::Equal)? {
            Some(parse_assignment(parser, false)?)
        } else {
            None
        };
        let end = parser.prev.as_ref().unwrap().span.end;
        decls.push(VarDeclarator {
            pattern,
            init,
            span: Span::new(start, end),
        });
        if !parser.eat_punct(TokenKind::Comma)? {
            break;
        }
    }
    Ok(decls)
}

/// Converts a for-head init into the for-in/for-of binding.
/// `allow_var_init` admits the Annex B.2.6 form `for (var x = init in obj)`
/// in sloppy code; the initializer is otherwise a syntax error.
fn for_binding_from_init(
    parser: &mut Parser,
    init: Option<ForInit>,
    allow_var_init: bool,
) -> Result<ForBinding, JsError> {
    match init {
        Some(ForInit::Expr(expr)) => {
            parser.check_assignment_target(&expr, syntax::AssignOp::Assign)?;
            Ok(ForBinding::Expr(expr))
        }
        Some(ForInit::VarDecl { kind, decls }) => {
            // A for-in/of head has a single ForBinding (spec 14.7.5), so
            // `for (let x, y in obj)` is an early error.
            if decls.len() > 1 {
                return Err(parser.error_at(
                    decls[1].span.start,
                    "Invalid multiple bindings in for-in/of declaration",
                ));
            }
            let decl = decls
                .into_iter()
                .next()
                .ok_or_else(|| parser.error_at(0, "Invalid for-in/of declaration"))?;
            if decl.init.is_some() {
                let annex_b = allow_var_init
                    && !parser.strict
                    && kind == VarDeclKind::Var
                    && matches!(decl.pattern, BindingPattern::Ident(_));
                if !annex_b {
                    return Err(parser.error_at(
                        decl.span.start,
                        "Invalid initializer in for-in/of declaration",
                    ));
                }
            }
            Ok(ForBinding::VarDecl {
                kind,
                pattern: decl.pattern,
                init: decl.init,
            })
        }
        None => Err(parser.error_at(0, "Invalid for-in/of head")),
    }
}

fn parse_continue(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `continue`
    let tok = parser.peek()?.clone();
    let label = if !tok.line_break_before && is_label_identifier(parser, &tok) {
        let (name, _) = parser.parse_identifier()?;
        Some(name)
    } else {
        None
    };
    parser.expect_semicolon()?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::Continue(label),
    })
}

fn parse_break(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `break`
    let tok = parser.peek()?.clone();
    let label = if !tok.line_break_before && is_label_identifier(parser, &tok) {
        let (name, _) = parser.parse_identifier()?;
        Some(name)
    } else {
        None
    };
    parser.expect_semicolon()?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::Break(label),
    })
}

fn is_label_identifier(parser: &Parser, tok: &Token) -> bool {
    let TokenKind::Identifier(atom) = tok.kind else {
        return false;
    };
    if from_identifier(atom).is_some() {
        return false;
    }
    if parser.strict && syntax::keywords::is_future_reserved_word(atom) {
        return false;
    }
    true
}

fn parse_return(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `return`
    if !parser.in_function {
        return Err(parser.error_at(start, "Illegal return statement"));
    }
    if parser.in_static_block {
        return Err(parser.error_at(
            start,
            "Illegal return statement in a class static initialization block",
        ));
    }
    let argument = if parser.peek()?.line_break_before {
        None
    } else if can_start_expression(parser.peek()?.kind.clone()) {
        Some(parse_expression(parser, true)?)
    } else {
        None
    };
    parser.expect_semicolon()?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::Return(argument),
    })
}

fn parse_with(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `with`
    if parser.strict {
        return Err(parser.error_at(start, "Strict mode code may not include a with statement"));
    }
    parser.expect_punct(TokenKind::LeftParen)?;
    let object = parse_expression(parser, true)?;
    parser.expect_punct(TokenKind::RightParen)?;
    let body = Box::new(parse_statement_with(parser, false, false)?);
    if is_labelled_function(&body) {
        return Err(parser.error_at(
            body.span.start,
            "A labelled function declaration is not allowed in statement position",
        ));
    }
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::With { object, body },
    })
}

fn parse_switch(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `switch`
    parser.expect_punct(TokenKind::LeftParen)?;
    let discriminant = parse_expression(parser, true)?;
    parser.expect_punct(TokenKind::RightParen)?;
    parser.expect_punct(TokenKind::LeftBrace)?;
    let mut cases: Vec<SwitchCase> = Vec::new();
    let mut saw_default = false;
    parser.push_scope();
    parser.scopes.last_mut().unwrap().is_block = true;
    let saved_vars = std::mem::take(&mut parser.list_vars);
    while !parser.at_punct(TokenKind::RightBrace)? {
        let case_start = parser.peek()?.span.start;
        let test = if parser.eat_keyword(Keyword::Case)? {
            let test = parse_expression(parser, true)?;
            parser.expect_punct(TokenKind::Colon)?;
            Some(test)
        } else if parser.eat_keyword(Keyword::Default)? {
            if saw_default {
                return Err(
                    parser.error_at(case_start, "Multiple default clauses in switch statement")
                );
            }
            saw_default = true;
            parser.expect_punct(TokenKind::Colon)?;
            None
        } else {
            let tok = parser.peek()?.clone();
            return Err(parser.unexpected(&tok));
        };
        let consequent = parse_switch_consequents(parser)?;
        cases.push(SwitchCase {
            test,
            consequent,
            span: Span::new(case_start, parser.prev.as_ref().unwrap().span.end),
        });
    }
    parser.expect_punct(TokenKind::RightBrace)?;
    parser.pop_scope();
    parser.list_vars.extend(saved_vars);
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::Switch {
            discriminant,
            cases,
        },
    })
}

/// Statements between switch cases, ending at `case`/`default`/`}`. The
/// switch's statement list is shared across cases (one CaseBlock scope).
fn parse_switch_consequents(parser: &mut Parser) -> Result<Vec<Stmt>, JsError> {
    let mut stmts = Vec::new();
    loop {
        let kind = parser.peek()?.kind.clone();
        if matches!(kind, TokenKind::Eof)
            || parser.at_punct(TokenKind::RightBrace)?
            || parser.at_keyword(Keyword::Case)?
            || parser.at_keyword(Keyword::Default)?
        {
            break;
        }
        let stmt = parse_statement(parser, true)?;
        // A UsingDeclaration directly in a case/default clause's statement
        // list is an early error (spec 15.14.2); a nested block is fine.
        if matches!(stmt.kind, StmtKind::UsingDecl { .. }) {
            return Err(parser.error_at(
                stmt.span.start,
                "using declarations are not allowed in a switch case clause",
            ));
        }
        stmts.push(stmt);
    }
    Ok(stmts)
}

fn parse_throw(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `throw`
    if parser.peek()?.line_break_before {
        return Err(parser.error_at(start, "Illegal newline after throw"));
    }
    let argument = parse_expression(parser, true)?;
    parser.expect_semicolon()?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::Throw(argument),
    })
}

fn parse_try(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `try`
    let block = parse_block(parser)?;
    let mut handler = None;
    let mut finalizer = None;
    if parser.eat_keyword(Keyword::Catch)? {
        let catch_start = parser.prev.as_ref().unwrap().span.start;
        let param = if parser.eat_punct(TokenKind::LeftParen)? {
            let pattern = parser.parse_binding_pattern()?;
            for name in bound_names(&pattern) {
                parser.check_binding_name(name, catch_start)?;
            }
            parser.expect_punct(TokenKind::RightParen)?;
            Some(pattern)
        } else {
            None
        };
        parser.push_scope();
        parser.scopes.last_mut().unwrap().is_catch = true;
        if let Some(pattern) = &param {
            for name in bound_names(pattern) {
                parser.declare_catch_param(name, catch_start)?;
            }
        }
        let body = parse_block(parser)?;
        parser.pop_scope();
        // spec 15.1.8: CatchParameter bound names must not occur in the
        // block's LexicallyDeclaredNames (this also covers `let`/`class`/
        // `function` declarations anywhere inside the block).
        if let Some(pattern) = &param {
            let mut declared = Vec::new();
            lexically_declared_names(&body.stmts, &mut declared);
            for name in bound_names(pattern) {
                if declared.contains(&name) {
                    return Err(
                        parser.error_at(catch_start, "Identifier has already been declared")
                    );
                }
            }
        }
        handler = Some(CatchClause {
            param,
            body: body.clone(),
            span: Span::new(catch_start, body.span.end),
        });
    }
    if parser.eat_keyword(Keyword::Finally)? {
        finalizer = Some(parse_block(parser)?);
    }
    if handler.is_none() && finalizer.is_none() {
        return Err(parser.error_at(start, "Missing catch or finally after try"));
    }
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::Try {
            block,
            handler,
            finalizer,
        },
    })
}

/// LexicallyDeclaredNames of a statement list (spec 15.2.1), used by the
/// catch-parameter early error: `let`/`const`/`class`/`using`/function
/// declarations count when they are direct statement-list items or the body
/// of an if/while/do/for/with statement, but names inside plain nested
/// blocks, try/catch/finally, switch cases, or labelled statements do not
/// (spec 15.2.1.1; a nested block shadows the catch parameter instead of
/// clashing with it).
fn lexically_declared_names(stmts: &[Stmt], out: &mut Vec<AtomId>) {
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::VarDecl { kind, decls, .. } if *kind != VarDeclKind::Var => {
                for decl in decls {
                    out.extend(bound_names(&decl.pattern));
                }
            }
            StmtKind::UsingDecl { decls, .. } => {
                for decl in decls {
                    out.extend(bound_names(&decl.pattern));
                }
            }
            StmtKind::FunctionDecl(function) => {
                // A statement-position declaration (`if (x) function f(){}`)
                // is Annex B var-scoped: it is not a LexicallyDeclaredName of
                // the enclosing statement list, so it may share a catch
                // parameter's name (B.3.3). Block-level declarations clash.
                if !function.statement_position
                    && let Some(name) = function.name
                {
                    out.push(name);
                }
            }
            StmtKind::ClassDecl(class) => {
                if let Some(name) = class.name {
                    out.push(name);
                }
            }
            StmtKind::If {
                consequent,
                alternate,
                ..
            } => {
                lexically_declared_names(std::slice::from_ref(consequent), out);
                if let Some(alternate) = alternate {
                    lexically_declared_names(std::slice::from_ref(alternate), out);
                }
            }
            StmtKind::While { body, .. }
            | StmtKind::DoWhile { body, .. }
            | StmtKind::With { body, .. } => {
                lexically_declared_names(std::slice::from_ref(body), out);
            }
            StmtKind::For { body, .. } => {
                lexically_declared_names(std::slice::from_ref(body), out);
            }
            StmtKind::ForIn { body, .. } | StmtKind::ForOf { body, .. } => {
                lexically_declared_names(std::slice::from_ref(body), out);
            }
            _ => {}
        }
    }
}

/// `function name ( params ) { body }` — the name is required for
/// declarations. `annex_b` marks the Annex B statement positions (`if` clause
/// bodies and labelled statements) where a plain `function` declaration is
/// accepted in sloppy mode (spec B.3.4/B.3.2); generator and async
/// declarations are never accepted at Statement position.
fn parse_function_declaration(
    parser: &mut Parser,
    is_async: bool,
    statement_position: bool,
    annex_b: bool,
) -> Result<Stmt, JsError> {
    // The span covers the whole declaration; an async declaration's caller
    // has already consumed the `async` keyword, so it is `parser.prev` there.
    let start = if is_async {
        parser.prev.as_ref().unwrap().span.start
    } else {
        parser.peek()?.span.start
    };
    parser.next()?; // `function`
    let is_generator = parser.eat_punct(TokenKind::Star)?;
    if statement_position && !(annex_b && !parser.strict && !is_generator && !is_async) {
        return Err(parser.error_at(
            start,
            "Function declarations are not allowed in statement position",
        ));
    }
    if !parser.at_identifier()? {
        let tok = parser.peek()?.clone();
        return Err(parser.unexpected(&tok));
    }
    let (name, name_start) = parser.parse_identifier()?;
    parser.check_binding_name(name, name_start)?;
    parser.declare_function(name, name_start, statement_position, is_async, is_generator)?;
    parser.expect_punct(TokenKind::LeftParen)?;
    // Params parse with the function's own [Yield, Await] grammar: async
    // declarations reserve `await` in their formal parameters (spec 15.8.1),
    // and a module's top-level `await` never leaks into them (spec
    // 15.2.1.1: FormalParameters are [~Await]).
    let saved = (parser.in_generator, parser.in_async, parser.top_level_await);
    parser.in_generator = is_generator;
    parser.in_async = is_async;
    parser.top_level_await = false;
    let params = crate::expr::parse_parameter_list(parser)?;
    (parser.in_generator, parser.in_async, parser.top_level_await) = saved;
    crate::expr::check_duplicate_params(parser, &params, false)?;
    crate::expr::check_function_params(parser, &params, is_async, is_generator)?;
    let (body, strict) = crate::expr::parse_function_body_block(
        parser,
        is_async,
        is_generator,
        &params,
        false,
        false,
        false,
    )?;
    // A strict body (enclosing strict code or a `"use strict"` directive)
    // forbids `eval`/`arguments` as the declaration's name (spec 15.4.1).
    if strict {
        crate::expr::check_function_name_strict(parser, Some(name), name_start)?;
    }
    let end = body.span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::FunctionDecl(syntax::Function {
            span: Span::new(start, end),
            name: Some(name),
            params,
            body,
            is_async,
            is_generator,
            statement_position,
        }),
    })
}
