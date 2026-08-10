//! Statement and declaration parsing (spec ch. 14).

use crux::{AtomId, JsError, Span, intern_utf8};
use syntax::keywords::{Keyword, from_identifier};
use syntax::{
    BindingPattern, Block, CatchClause, ForBinding, ForInit, Stmt, StmtKind, SwitchCase, Token,
    TokenKind, VarDeclKind, VarDeclarator,
};

use crate::expr::{can_start_expression, parse_assignment, parse_expression};
use crate::parser::{LabelInfo, Parser};

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
        stmts.push(parse_statement(parser)?);
    }
    Ok(stmts)
}

pub(crate) fn parse_statement(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.peek()?.span.start;
    let kind = match parser.peek()?.kind.clone() {
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
        TokenKind::Identifier(atom) => match from_identifier(atom) {
            Some(Keyword::Var) => return parse_var_statement(parser, VarDeclKind::Var),
            Some(Keyword::Const) => return parse_var_statement(parser, VarDeclKind::Const),
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
            Some(Keyword::Function) => return parse_function_declaration(parser, false),
            Some(Keyword::Class) => {
                let start = parser.next()?.span.start;
                let class = crate::class::parse_class(parser, start, true)?;
                let end = class.span.end;
                return Ok(Stmt {
                    span: Span::new(start, end),
                    kind: StmtKind::ClassDecl(class),
                });
            }
            _ if atom == intern_utf8("let")
                && is_let_declaration_start(parser.peek2()?.kind.clone()) =>
            {
                return parse_var_statement(parser, VarDeclKind::Let);
            }
            _ if atom == intern_utf8("using")
                && is_using_binding_start(parser.peek2()?.kind.clone())
                && !parser.peek2()?.line_break_before =>
            {
                return parse_using_declaration(parser, false);
            }
            _ if atom == intern_utf8("await")
                && (parser.in_async || parser.top_level_await)
                && parser.peek2()?.kind == TokenKind::Identifier(intern_utf8("using"))
                && !parser.peek2()?.line_break_before
                && is_using_binding_start(parser.peek3()?.kind.clone())
                && !parser.peek3()?.line_break_before =>
            {
                parser.next()?; // `await`
                return parse_using_declaration(parser, true);
            }
            _ if atom == intern_utf8("async")
                && parser.peek2()?.kind == TokenKind::Identifier(intern_utf8("function"))
                && !parser.peek2()?.line_break_before =>
            {
                parser.next()?; // `async`
                return parse_function_declaration(parser, true);
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

/// `let` starts a declaration when followed by an identifier, `[`, or `{`.
fn is_let_declaration_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier(_) | TokenKind::LeftBracket | TokenKind::LeftBrace
    )
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
    parser.labels.push(LabelInfo {
        name,
        pending_continues: Vec::new(),
    });
    let body = Box::new(parse_statement(parser)?);
    let info = parser.labels.pop().unwrap();
    let is_loop = matches!(
        body.kind,
        StmtKind::While { .. }
            | StmtKind::DoWhile { .. }
            | StmtKind::For { .. }
            | StmtKind::ForIn { .. }
            | StmtKind::ForOf { .. }
    );
    for span in info.pending_continues {
        if !is_loop {
            return Err(parser.error_at(span, "Illegal continue statement"));
        }
    }
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::Labeled { label: name, body },
    })
}

fn parse_block(parser: &mut Parser) -> Result<Block, JsError> {
    let start = parser.next()?.span.start; // '{'
    let saved_vars = std::mem::take(&mut parser.list_vars);
    parser.push_scope();
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
    let mut decls = Vec::new();
    loop {
        let decl_start = parser.peek()?.span.start;
        let (name, name_start) = parser.parse_identifier()?;
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
                VarDeclKind::Let | VarDeclKind::Const => parser.declare_lexical(name, start)?,
                VarDeclKind::Using | VarDeclKind::AwaitUsing => {
                    if name == intern_utf8("let") {
                        return Err(
                            parser.error_at(start, "let is disallowed as a lexically bound name")
                        );
                    }
                    parser.declare_lexical(name, start)?;
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
    let consequent = Box::new(parse_statement(parser)?);
    let alternate = if parser.eat_keyword(Keyword::Else)? {
        Some(Box::new(parse_statement(parser)?))
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
    parser.loop_depth += 1;
    let body = Box::new(parse_statement(parser)?);
    parser.loop_depth -= 1;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::While { test, body },
    })
}

fn parse_do_while(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `do`
    parser.loop_depth += 1;
    let body = Box::new(parse_statement(parser)?);
    parser.loop_depth -= 1;
    parser.expect_keyword(Keyword::While)?;
    parser.expect_punct(TokenKind::LeftParen)?;
    let test = parse_expression(parser, true)?;
    parser.expect_punct(TokenKind::RightParen)?;
    parser.expect_semicolon()?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::DoWhile { body, test },
    })
}

fn parse_for(parser: &mut Parser) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `for`
    // `for await ( … of … )` — only in async functions.
    let mut is_await = false;
    if parser.at_contextual("await")? && parser.peek2()?.kind == TokenKind::LeftParen {
        if !parser.in_async {
            let at = parser.peek()?.span.start;
            return Err(parser.error_at(at, "for await is only allowed in async functions"));
        }
        parser.next()?; // `await`
        is_await = true;
    }
    parser.expect_punct(TokenKind::LeftParen)?;

    let (init, init_empty) = if parser.eat_punct(TokenKind::Semicolon)? {
        (None, true)
    } else if parser.at_keyword(Keyword::Var)? {
        let kind = VarDeclKind::Var;
        parser.next()?;
        (
            Some(ForInit::VarDecl {
                kind,
                decls: parse_for_declarators(parser, kind)?,
            }),
            false,
        )
    } else if parser.at_keyword(Keyword::Const)? {
        let kind = VarDeclKind::Const;
        parser.next()?;
        (
            Some(ForInit::VarDecl {
                kind,
                decls: parse_for_declarators(parser, kind)?,
            }),
            false,
        )
    } else if parser.at_contextual("let")? && is_let_declaration_start(parser.peek2()?.kind.clone())
    {
        let kind = VarDeclKind::Let;
        parser.next()?;
        (
            Some(ForInit::VarDecl {
                kind,
                decls: parse_for_declarators(parser, kind)?,
            }),
            false,
        )
    } else if parser.at_contextual("using")?
        && is_using_binding_start(parser.peek2()?.kind.clone())
        && !parser.peek2()?.line_break_before
        && parser.peek2()?.kind != TokenKind::Identifier(intern_utf8("of"))
    {
        // `for (using x of y)` / `for (using x = 0; …)`. The `of` lookahead
        // keeps `for (using of y)` an expression-headed for-of (spec 14.7.5).
        let kind = VarDeclKind::Using;
        parser.next()?;
        (
            Some(ForInit::VarDecl {
                kind,
                decls: parse_for_declarators(parser, kind)?,
            }),
            false,
        )
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
        (
            Some(ForInit::VarDecl {
                kind,
                decls: parse_for_declarators(parser, kind)?,
            }),
            false,
        )
    } else {
        (Some(ForInit::Expr(parse_expression(parser, false)?)), false)
    };

    if parser.at_keyword(Keyword::In)? {
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
        let left = for_binding_from_init(parser, init, true)?;
        let right = parse_expression(parser, true)?;
        parser.expect_punct(TokenKind::RightParen)?;
        parser.loop_depth += 1;
        let body = Box::new(parse_statement(parser)?);
        parser.loop_depth -= 1;
        let end = parser.prev.as_ref().unwrap().span.end;
        return Ok(Stmt {
            span: Span::new(start, end),
            kind: StmtKind::ForIn { left, right, body },
        });
    }
    if parser.at_contextual("of")? {
        if is_await && parser.peek()?.line_break_before {
            return Err(parser.error_at(start, "Unexpected line break after for await"));
        }
        parser.next()?;
        let left = for_binding_from_init(parser, init, false)?;
        let right = parse_assignment(parser, true)?;
        parser.expect_punct(TokenKind::RightParen)?;
        parser.loop_depth += 1;
        let body = Box::new(parse_statement(parser)?);
        parser.loop_depth -= 1;
        let end = parser.prev.as_ref().unwrap().span.end;
        return Ok(Stmt {
            span: Span::new(start, end),
            kind: StmtKind::ForOf {
                left,
                right,
                body,
                is_await,
            },
        });
    }

    // Classic `for ( init ; test ; update )`. When the init slot was empty,
    // its `;` is already consumed.
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
    parser.loop_depth += 1;
    let body = Box::new(parse_statement(parser)?);
    parser.loop_depth -= 1;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Stmt {
        span: Span::new(start, end),
        kind: StmtKind::For {
            init,
            test,
            update,
            body,
        },
    })
}

/// Declarators in a for-head: `[~In]` throughout (the `in` of a for-in head
/// must not be consumed by an initializer), and exactly one binding when the
/// head turns out to be for-in/for-of.
fn parse_for_declarators(
    parser: &mut Parser,
    kind: VarDeclKind,
) -> Result<Vec<VarDeclarator>, JsError> {
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
            VarDeclKind::Using | VarDeclKind::AwaitUsing => parser.declare_lexical(name, start)?,
        }
    }
    let init = if parser.eat_punct(TokenKind::Equal)? {
        Some(parse_assignment(parser, false)?)
    } else {
        None
    };
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(vec![VarDeclarator {
        pattern,
        init,
        span: Span::new(start, end),
    }])
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
    match label {
        None => {
            if parser.loop_depth == 0 {
                return Err(parser.error_at(start, "Illegal continue statement"));
            }
        }
        Some(name) => {
            // The label must exist and, once the labeled statement is known,
            // must target an iteration statement.
            let mut found = false;
            for info in parser.labels.iter_mut().rev() {
                if info.name == name {
                    info.pending_continues.push(start);
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(parser.error_at(start, "Undefined label"));
            }
        }
    }
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
    match label {
        None => {
            if parser.loop_depth == 0 && parser.switch_depth == 0 {
                return Err(parser.error_at(start, "Illegal break statement"));
            }
        }
        Some(name) => {
            if !parser.labels.iter().rev().any(|info| info.name == name) {
                return Err(parser.error_at(start, "Undefined label"));
            }
        }
    }
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
    let body = Box::new(parse_statement(parser)?);
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
    parser.switch_depth += 1;
    parser.push_scope();
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
    parser.switch_depth -= 1;
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
        stmts.push(parse_statement(parser)?);
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
        if let Some(pattern) = &param {
            for name in bound_names(pattern) {
                parser.declare_lexical(name, catch_start)?;
            }
        }
        let body = parse_block(parser)?;
        parser.pop_scope();
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

/// `function name ( params ) { body }` — the name is required for
/// declarations.
fn parse_function_declaration(parser: &mut Parser, is_async: bool) -> Result<Stmt, JsError> {
    let start = parser.next()?.span.start; // `function`
    let is_generator = parser.eat_punct(TokenKind::Star)?;
    if !parser.at_identifier()? {
        let tok = parser.peek()?.clone();
        return Err(parser.unexpected(&tok));
    }
    let (name, name_start) = parser.parse_identifier()?;
    parser.check_binding_name(name, name_start)?;
    parser.declare_function(name, name_start)?;
    parser.expect_punct(TokenKind::LeftParen)?;
    let params = crate::expr::parse_parameter_list(parser)?;
    crate::expr::check_duplicate_params(parser, &params, false)?;
    let body = crate::expr::parse_function_body_block(
        parser,
        is_async,
        is_generator,
        &params,
        false,
        false,
    )?;
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
        }),
    })
}
