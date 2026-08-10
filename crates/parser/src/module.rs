//! Modules: import and export declarations (spec 16.2).

use crux::{AtomId, JsError, JsString, Span, intern_utf8};
use syntax::keywords::{Keyword, from_identifier};
use syntax::{
    AttributeKey, Class, ExportDecl, ExportDefault, ExportName, ExportSpecifier, ImportDecl,
    ImportEntry, ModuleItem, TokenKind,
};

use crate::expr::parse_function_expression;
use crate::parser::Parser;

/// Parses the top-level item list of a module.
pub(crate) fn parse_module_items(parser: &mut Parser) -> Result<Vec<ModuleItem>, JsError> {
    let mut items = Vec::new();
    while !matches!(parser.peek()?.kind, TokenKind::Eof) {
        items.push(parse_module_item(parser)?);
    }
    Ok(items)
}

fn parse_module_item(parser: &mut Parser) -> Result<ModuleItem, JsError> {
    if parser.at_keyword(Keyword::Import)? {
        // `import(...)` / `import.meta` are expressions, not declarations.
        let next = parser.peek2()?.kind.clone();
        if !matches!(next, TokenKind::LeftParen | TokenKind::Dot) {
            return parse_import_declaration(parser).map(ModuleItem::Import);
        }
    }
    if parser.at_keyword(Keyword::Export)? {
        return parse_export_declaration(parser).map(ModuleItem::Export);
    }
    parser.parse_statement_public().map(ModuleItem::Stmt)
}

fn parse_import_declaration(parser: &mut Parser) -> Result<ImportDecl, JsError> {
    let start = parser.next()?.span.start; // `import`
    let mut entries: Vec<ImportEntry> = Vec::new();

    // `import "mod" with { … };` — a side-effect-only import.
    if matches!(parser.peek()?.kind, TokenKind::StringLiteral { .. }) {
        let tok = parser.next()?;
        let specifier = string_value(tok);
        let attributes = parse_with_clause(parser)?;
        parser.expect_semicolon()?;
        let end = parser.prev.as_ref().unwrap().span.end;
        return Ok(ImportDecl {
            span: Span::new(start, end),
            specifier,
            entries,
            attributes,
        });
    }

    if parser.eat_punct(TokenKind::Star)? {
        // `import * as ns from …`.
        parser.expect_contextual("as")?;
        let local = parse_imported_binding(parser)?;
        entries.push(ImportEntry::Namespace {
            local: local.0,
            span: local.1,
        });
    } else if parser.at_punct(TokenKind::LeftBrace)? {
        // `import { … } from …`.
        entries.extend(parse_named_imports(parser)?);
    } else {
        // `import default from …` with optional `, {…}` / `, * as ns`.
        let local = parse_imported_binding(parser)?;
        entries.push(ImportEntry::Default {
            local: local.0,
            span: local.1,
        });
        if parser.eat_punct(TokenKind::Comma)? {
            if parser.eat_punct(TokenKind::Star)? {
                parser.expect_contextual("as")?;
                let local = parse_imported_binding(parser)?;
                entries.push(ImportEntry::Namespace {
                    local: local.0,
                    span: local.1,
                });
            } else {
                entries.extend(parse_named_imports(parser)?);
            }
        }
    }

    parser.expect_contextual("from")?;
    let specifier = parse_module_specifier(parser)?;
    let attributes = parse_with_clause(parser)?;
    parser.expect_semicolon()?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(ImportDecl {
        span: Span::new(start, end),
        specifier,
        entries,
        attributes,
    })
}

/// `import { a, b as c, "str" as d } from …` — the `{` is consumed here.
fn parse_named_imports(parser: &mut Parser) -> Result<Vec<ImportEntry>, JsError> {
    parser.expect_punct(TokenKind::LeftBrace)?;
    let mut entries = Vec::new();
    while !parser.at_punct(TokenKind::RightBrace)? {
        let span_start = parser.peek()?.span.start;
        // The first name is a ModuleExportName (any identifier or string).
        if !is_module_export_name_start(parser.peek()?.kind.clone()) {
            let tok = parser.peek()?.clone();
            return Err(parser.unexpected(&tok));
        }
        // `ModuleExportName as ImportedBinding` — the alias is the only
        // specifier form whose second token is the contextual `as`.
        if parser.peek2()?.kind == TokenKind::Identifier(intern_utf8("as")) {
            // `exported as local`.
            let imported = parse_module_export_name(parser)?;
            parser.expect_contextual("as")?;
            let local = parse_imported_binding(parser)?;
            let end = local.1.end;
            entries.push(ImportEntry::Named {
                imported,
                local: local.0,
                span: Span::new(span_start, end),
            });
        } else {
            // A plain binding: `{ x }` — the imported name is the binding.
            let local = parse_imported_binding(parser)?;
            let end = local.1.end;
            entries.push(ImportEntry::Named {
                imported: ExportName::Ident(local.0),
                local: local.0,
                span: Span::new(span_start, end),
            });
        }
        if !parser.eat_punct(TokenKind::Comma)? {
            break;
        }
    }
    parser.expect_punct(TokenKind::RightBrace)?;
    Ok(entries)
}

fn is_module_export_name_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier(_) | TokenKind::StringLiteral { .. }
    )
}

/// An `ImportedBinding`: a binding identifier, declared lexically.
fn parse_imported_binding(parser: &mut Parser) -> Result<(AtomId, Span), JsError> {
    let tok = parser.peek()?.clone();
    let (name, start) = parser.parse_identifier()?;
    parser.check_binding_name(name, start)?;
    parser.declare_lexical(name, start)?;
    Ok((name, Span::new(start, tok.span.end)))
}

fn parse_module_specifier(parser: &mut Parser) -> Result<JsString, JsError> {
    let tok = parser.next()?;
    let TokenKind::StringLiteral { value, .. } = tok.kind else {
        return Err(parser.unexpected(&tok));
    };
    Ok(value)
}

fn parse_module_export_name(parser: &mut Parser) -> Result<ExportName, JsError> {
    let tok = parser.peek()?.clone();
    match tok.kind {
        TokenKind::Identifier(atom) => {
            parser.next()?;
            Ok(ExportName::Ident(atom))
        }
        TokenKind::StringLiteral { value, .. } => {
            parser.next()?;
            Ok(ExportName::Str(value))
        }
        _ => Err(parser.unexpected(&tok)),
    }
}

/// `with { "type": "json" }` — import attributes (spec 16.2.4).
fn parse_with_clause(parser: &mut Parser) -> Result<Vec<(AttributeKey, JsString)>, JsError> {
    if !parser.eat_contextual("with")? {
        return Ok(Vec::new());
    }
    parser.expect_punct(TokenKind::LeftBrace)?;
    let mut out = Vec::new();
    while !parser.at_punct(TokenKind::RightBrace)? {
        let key = match parser.peek()?.kind.clone() {
            TokenKind::Identifier(atom) => {
                parser.next()?;
                AttributeKey::Ident(atom)
            }
            TokenKind::StringLiteral { value, .. } => {
                parser.next()?;
                AttributeKey::Str(value)
            }
            _ => {
                let tok = parser.peek()?.clone();
                return Err(parser.unexpected(&tok));
            }
        };
        parser.expect_punct(TokenKind::Colon)?;
        let value = parse_module_specifier(parser)?;
        out.push((key, value));
        if !parser.eat_punct(TokenKind::Comma)? {
            break;
        }
    }
    parser.expect_punct(TokenKind::RightBrace)?;
    Ok(out)
}

fn parse_export_declaration(parser: &mut Parser) -> Result<ExportDecl, JsError> {
    let start = parser.next()?.span.start; // `export`

    if parser.eat_keyword(Keyword::Default)? {
        return Ok(ExportDecl::Default(Box::new(parse_export_default(parser)?)));
    }

    // `export * from …` / `export * as ns from …`.
    if parser.eat_punct(TokenKind::Star)? {
        let namespace = if parser.eat_contextual("as")? {
            Some(parse_module_export_name(parser)?)
        } else {
            None
        };
        parser.expect_contextual("from")?;
        let specifier = parse_module_specifier(parser)?;
        let attributes = parse_with_clause(parser)?;
        parser.expect_semicolon()?;
        let end = parser.prev.as_ref().unwrap().span.end;
        return Ok(ExportDecl::From {
            specifiers: Vec::new(),
            namespace,
            specifier,
            attributes,
            span: Span::new(start, end),
        });
    }

    // `export { a, b as c } [from …];`.
    if parser.at_punct(TokenKind::LeftBrace)? {
        let specifiers = parse_export_specifier_list(parser)?;
        if parser.eat_contextual("from")? {
            let specifier = parse_module_specifier(parser)?;
            let attributes = parse_with_clause(parser)?;
            parser.expect_semicolon()?;
            let end = parser.prev.as_ref().unwrap().span.end;
            return Ok(ExportDecl::From {
                specifiers,
                namespace: None,
                specifier,
                attributes,
                span: Span::new(start, end),
            });
        }
        parser.expect_semicolon()?;
        let end = parser.prev.as_ref().unwrap().span.end;
        return Ok(ExportDecl::Named {
            specifiers,
            span: Span::new(start, end),
        });
    }

    // `export var/let/const/function/class …`.
    let stmt = parser.parse_statement_public()?;
    match stmt.kind {
        syntax::StmtKind::VarDecl { .. }
        | syntax::StmtKind::FunctionDecl(_)
        | syntax::StmtKind::ClassDecl(_) => Ok(ExportDecl::Declaration(stmt)),
        _ => {
            let tok = parser.prev.as_ref().unwrap().clone();
            Err(parser.unexpected(&tok))
        }
    }
}

fn parse_export_default(parser: &mut Parser) -> Result<ExportDefault, JsError> {
    // `export default function …` / `export default async function …`.
    if parser.at_keyword(Keyword::Function)? {
        let expr = parse_function_expression(parser, false)?;
        let syntax::ExprKind::Function(function) = expr.kind else {
            unreachable!("function expression")
        };
        return Ok(ExportDefault::Function(function));
    }
    if parser.at_contextual("async")?
        && !parser.peek2()?.line_break_before
        && matches!(
            parser.peek2()?.kind,
            TokenKind::Identifier(ref atom) if from_identifier(*atom) == Some(Keyword::Function)
        )
    {
        parser.next()?; // `async`
        let expr = parse_function_expression(parser, true)?;
        let syntax::ExprKind::Function(function) = expr.kind else {
            unreachable!("function expression")
        };
        return Ok(ExportDefault::Function(function));
    }
    // `export default class …`.
    if parser.at_keyword(Keyword::Class)? {
        let class_start = parser.next()?.span.start;
        let class: Class = crate::class::parse_class(parser, class_start, false)?;
        return Ok(ExportDefault::Class(class));
    }
    // `export default AssignmentExpression ;`.
    let expr = crate::expr::parse_assignment(parser, true)?;
    parser.expect_semicolon()?;
    Ok(ExportDefault::Expr(expr))
}

/// `{ a, b as c }` — the `{` is consumed here.
fn parse_export_specifier_list(parser: &mut Parser) -> Result<Vec<ExportSpecifier>, JsError> {
    parser.expect_punct(TokenKind::LeftBrace)?;
    let mut out = Vec::new();
    while !parser.at_punct(TokenKind::RightBrace)? {
        let local = parse_module_export_name(parser)?;
        if parser.eat_contextual("as")? {
            let exported = parse_module_export_name(parser)?;
            out.push(ExportSpecifier::Alias { local, exported });
        } else {
            out.push(ExportSpecifier::Same(local));
        }
        if !parser.eat_punct(TokenKind::Comma)? {
            break;
        }
    }
    parser.expect_punct(TokenKind::RightBrace)?;
    Ok(out)
}

fn string_value(tok: syntax::Token) -> JsString {
    match tok.kind {
        TokenKind::StringLiteral { value, .. } => value,
        _ => unreachable!("validated by the caller"),
    }
}
