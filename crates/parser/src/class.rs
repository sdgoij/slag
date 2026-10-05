//! Class declarations and expressions (spec 15.7).

use crux::{JsError, Span, intern_utf8};
use syntax::keywords::Keyword;
use syntax::{
    AssignOp, BindingPattern, Block, Class, ClassElement, ClassElementName, Expr, ExprKind,
    Function, MemberExpr, MemberProperty, Stmt, StmtKind, TokenKind,
};

use crate::expr::{
    check_duplicate_params, is_property_name_start, parse_assignment, parse_expression,
    parse_function_body_block, parse_lhs, parse_parameter_list,
};
use crate::parser::{Parser, PrivateNameKind};

/// Parses a class tail (heritage + body) after the `class` keyword has been
/// consumed. `start` is the `class` token's position.
pub(crate) fn parse_class(
    parser: &mut Parser,
    start: u32,
    is_declaration: bool,
    decorators: Vec<Expr>,
) -> Result<Class, JsError> {
    // A class definition is always strict mode code (spec 15.7.3), so the
    // name is parsed under the strict reserved-word rules (`class let {}`,
    // escaped or not, is a SyntaxError).
    let saved_strict = parser.strict;
    let saved_private = std::mem::take(&mut parser.private_names);
    let saved_derived = parser.in_derived_class;
    parser.strict = true;
    parser.private_names.push(std::collections::HashMap::new());

    // The class name: required for declarations, optional for expressions.
    let name = if parser.at_identifier()? {
        let (name, name_start) = parser.parse_identifier()?;
        if is_declaration {
            parser.check_binding_name(name, name_start)?;
            parser.declare_lexical(name, name_start)?;
        }
        Some(name)
    } else {
        if is_declaration {
            let tok = parser.peek()?.clone();
            return Err(parser.unexpected(&tok));
        }
        None
    };
    parser.push_scope();
    if let Some(name) = name {
        parser.scopes.last_mut().unwrap().lexical.insert(name);
    }

    let heritage = if parser.eat_keyword(Keyword::Extends)? {
        // The heritage is evaluated with the class's own PrivateEnvironment
        // not yet in scope (spec 15.7.11), so the class's own private names
        // are not visible to it (enclosing classes' names still are).
        let own = parser.private_names.pop().expect("class private map");
        let result = parse_lhs(parser);
        parser.private_names.push(own);
        let heritage = result?;
        // ClassHeritage is a LeftHandSideExpression; an unparenthesized
        // arrow is not one (spec 15.7.4).
        if matches!(heritage.kind, syntax::ExprKind::Arrow { .. }) {
            return Err(parser.error_at(
                heritage.span.start,
                "Class heritage must be a left-hand-side expression",
            ));
        }
        Some(heritage)
    } else {
        None
    };
    parser.in_derived_class = heritage.is_some();

    parser.expect_punct(TokenKind::LeftBrace)?;
    let mut elements: Vec<ClassElement> = Vec::new();
    let mut constructor_count = 0usize;
    while !parser.at_punct(TokenKind::RightBrace)? {
        let element_start = parser.peek()?.span.start;
        for element in parse_class_element(parser)? {
            check_class_element(parser, &element, element_start, &mut constructor_count)?;
            elements.push(element);
        }
    }
    parser.expect_punct(TokenKind::RightBrace)?;
    let end = parser.prev.as_ref().unwrap().span.end;
    parser.pop_scope();
    parser.strict = saved_strict;
    parser.private_names = saved_private;
    parser.in_derived_class = saved_derived;

    Ok(Class {
        span: Span::new(start, end),
        name,
        heritage,
        decorators,
        elements,
    })
}

/// Parses a decorator list (`@expr @expr …`) before a class or class element
/// (the decorators proposal); the expressions are returned in source order for
/// evaluation at class-definition time.
pub(crate) fn parse_decorators(parser: &mut Parser) -> Result<Vec<Expr>, JsError> {
    let mut decorators = Vec::new();
    while parser.eat_punct(TokenKind::At)? {
        let expr = if parser.at_punct(TokenKind::LeftParen)? {
            // `@( Expression )`
            parser.next()?;
            let inner = parse_expression(parser, true)?;
            parser.expect_punct(TokenKind::RightParen)?;
            inner
        } else {
            // `@ DecoratorMemberExpression` / `@ DecoratorCallExpression`:
            // `parse_lhs` consumes the whole member/call chain.
            parse_lhs(parser)?
        };
        decorators.push(expr);
    }
    Ok(decorators)
}

/// Whether a plain method is the constructor: an instance method named
/// `constructor` that is not a special method.
fn is_plain_constructor(name: &ClassElementName, function: &Function) -> bool {
    is_name(name, "constructor") && !function.is_async && !function.is_generator
}

/// Whether an element name's PropName equals `text` — the identifier form or
/// a string literal of the same value (spec 15.7.5 PropName).
fn is_name(name: &ClassElementName, text: &str) -> bool {
    match name {
        ClassElementName::Property(syntax::PropertyName::Ident(atom)) => atom == &intern_utf8(text),
        ClassElementName::Property(syntax::PropertyName::Str(value)) => {
            value == &crux::JsString::from_utf8(text)
        }
        _ => false,
    }
}

/// The element-level early errors a class body enforces on each parsed
/// element (constructor count, the `constructor`/`prototype` name bans).
fn check_class_element(
    parser: &Parser,
    element: &ClassElement,
    element_start: u32,
    constructor_count: &mut usize,
) -> Result<(), JsError> {
    match element {
        ClassElement::Method {
            is_static: false,
            name,
            function,
            ..
        } if is_plain_constructor(name, function) => {
            *constructor_count += 1;
            if *constructor_count > 1 {
                return Err(parser.error_at(element_start, "A class may only have one constructor"));
            }
        }
        ClassElement::Field {
            is_static: false,
            name,
            ..
        } if is_name(name, "constructor") => {
            return Err(parser.error_at(element_start, "Class field may not be named constructor"));
        }
        ClassElement::Field {
            is_static: true,
            name,
            ..
        } if is_name(name, "prototype") || is_name(name, "constructor") => {
            return Err(parser.error_at(
                element_start,
                "Static class field may not be named prototype or constructor",
            ));
        }
        ClassElement::Method {
            is_static: true,
            name,
            ..
        }
        | ClassElement::Get {
            is_static: true,
            name,
            ..
        }
        | ClassElement::Set {
            is_static: true,
            name,
            ..
        } if is_name(name, "prototype") => {
            return Err(parser.error_at(
                element_start,
                "Static class method may not be named prototype",
            ));
        }
        _ => {}
    }
    Ok(())
}

/// Whether `static` at the current position is the class-element prefix
/// rather than an element named `static`.
fn static_is_prefix(parser: &mut Parser) -> Result<bool, JsError> {
    Ok(matches!(
        parser.peek2()?.kind.clone(),
        TokenKind::LeftBrace
            | TokenKind::Star
            | TokenKind::LeftBracket
            | TokenKind::StringLiteral { .. }
            | TokenKind::NumericLiteral(_)
            | TokenKind::Identifier(_)
            | TokenKind::PrivateIdentifier(_)
    ))
}

/// Whether `accessor` at the current position is the field-accessor prefix
/// (`accessor ClassElementName …`) rather than an element named `accessor`.
fn accessor_is_prefix(parser: &mut Parser) -> Result<bool, JsError> {
    // The `[no LineTerminator here]` separates `accessor` from the name.
    Ok(!parser.peek2()?.line_break_before && is_class_name_start(parser.peek2()?.kind.clone()))
}

/// The getter/setter bodies of a public auto-accessor: `get () { return
/// this.#storage }` and `set (v) { this.#storage = v }`, where `#storage` is
/// the auto-accessor's hidden backing private field. `span` is the whole
/// `accessor …;` element (used as the accessors' `[[SourceText]]`).
fn synthesize_auto_accessor_bodies(
    span: Span,
    storage: crux::string::AtomId,
) -> (Block, BindingPattern, Block) {
    let this_storage = || Expr {
        span,
        kind: ExprKind::Member(MemberExpr {
            object: Box::new(Expr {
                span,
                kind: ExprKind::This,
            }),
            property: MemberProperty::Private(storage),
            property_token: Some(span),
            optional: false,
            span,
        }),
    };
    let get_body = Block {
        stmts: vec![Stmt {
            span,
            kind: StmtKind::Return(Some(this_storage())),
        }],
        span,
    };
    let param = intern_utf8("v");
    let set_body = Block {
        stmts: vec![Stmt {
            span,
            kind: StmtKind::Expr(Expr {
                span,
                kind: ExprKind::Assign {
                    op: AssignOp::Assign,
                    target: Box::new(this_storage()),
                    value: Box::new(Expr {
                        span,
                        kind: ExprKind::Ident(param),
                    }),
                },
            }),
        }],
        span,
    };
    (get_body, BindingPattern::Ident(param), set_body)
}

/// A fresh, user-unreachable private-name atom for an auto-accessor's backing
/// storage. A `#` private identifier can only be written with identifier
/// characters, so the `%…%` spelling cannot be produced by source.
fn auto_accessor_storage() -> crux::string::AtomId {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    intern_utf8(&format!("%auto-accessor{id}%"))
}

/// `= AssignmentExpression?` after a field/auto-accessor name, with the
/// `super`/`new.target` allowances of a field initializer.
fn parse_field_initializer(parser: &mut Parser) -> Result<Option<Expr>, JsError> {
    if !parser.eat_punct(TokenKind::Equal)? {
        return Ok(None);
    }
    // Field initializers may use `super` and `new.target` (the latter
    // resolves to undefined at runtime; it is not an early error).
    let saved = (
        parser.allow_super,
        parser.in_constructor,
        parser.in_field_initializer,
    );
    parser.allow_super = true;
    parser.in_constructor = false;
    parser.in_field_initializer = true;
    let value = parse_assignment(parser, true);
    (
        parser.allow_super,
        parser.in_constructor,
        parser.in_field_initializer,
    ) = saved;
    Ok(Some(value?))
}

fn parse_class_element(parser: &mut Parser) -> Result<Vec<ClassElement>, JsError> {
    if parser.eat_punct(TokenKind::Semicolon)? {
        return Ok(Vec::new());
    }
    // Decorators may precede any element (stage-3 proposal); they are
    // captured in source order for evaluation at class-definition time.
    let decorators = parse_decorators(parser)?;
    let is_static = if parser.at_contextual_unescaped("static")? && static_is_prefix(parser)? {
        parser.next()?;
        true
    } else {
        false
    };

    // `static { … }` — a class static initialization block.
    if is_static && parser.at_punct(TokenKind::LeftBrace)? {
        let body = parse_static_block(parser)?;
        return Ok(vec![ClassElement::StaticBlock(body)]);
    }

    // `accessor name …` — an auto-accessor (decorators proposal). It is
    // desugared here, into the elements the class machinery already
    // implements: a hidden private storage field plus a public get/set
    // accessor pair whose bodies read and write that field (see
    // `synthesize_auto_accessor_bodies`). A private auto-accessor
    // (`accessor #x`) is observably a private field — `this.#x` is its own
    // backing slot — so it desugars to a single private field.
    if parser.at_contextual_unescaped("accessor")? && accessor_is_prefix(parser)? {
        let accessor_start = parser.peek()?.span.start;
        parser.next()?; // `accessor`
        let name_start = parser.peek()?.span.start;
        let name = parse_class_element_name(parser)?;
        if matches!(name, ClassElementName::Private(_)) {
            declare_private_name(parser, &name, PrivateNameKind::Other, is_static)?;
            let init = parse_field_initializer(parser)?;
            parser.expect_semicolon()?;
            let end = parser.prev.as_ref().unwrap().span.end;
            return Ok(vec![ClassElement::Field {
                decorators,
                is_static,
                name,
                init,
                span: Span::new(accessor_start, end),
            }]);
        }
        // A public auto-accessor is a field-like element, so the field-name
        // early errors apply; the synthesized accessor pair would bypass them.
        if !is_static && is_name(&name, "constructor") {
            return Err(parser.error_at(name_start, "Class field may not be named constructor"));
        }
        if is_static && (is_name(&name, "prototype") || is_name(&name, "constructor")) {
            return Err(parser.error_at(
                name_start,
                "Static class field may not be named prototype or constructor",
            ));
        }
        let init = parse_field_initializer(parser)?;
        parser.expect_semicolon()?;
        let end = parser.prev.as_ref().unwrap().span.end;
        let span = Span::new(accessor_start, end);
        let storage = auto_accessor_storage();
        let storage_name = ClassElementName::Private(storage);
        declare_private_name(parser, &storage_name, PrivateNameKind::Other, is_static)?;
        let (get_body, set_param, set_body) = synthesize_auto_accessor_bodies(span, storage);
        // The auto-accessor's decorators ride on the getter, the first
        // publicly visible element of the synthesized trio (see S3).
        return Ok(vec![
            ClassElement::Field {
                decorators: Vec::new(),
                is_static,
                name: storage_name,
                init,
                span,
            },
            ClassElement::Get {
                decorators,
                is_static,
                name: name.clone(),
                body: get_body,
                span,
            },
            ClassElement::Set {
                decorators: Vec::new(),
                is_static,
                name,
                param: set_param,
                init: None,
                body: set_body,
                span,
            },
        ]);
    }

    // `*name() {}` — generator method.
    if parser.eat_punct(TokenKind::Star)? {
        let method_start = parser.prev.as_ref().unwrap().span.start; // `*`
        let name = parse_class_element_name(parser)?;
        check_special_constructor(parser, &name, is_static)?;
        let function = parse_class_method_tail(parser, method_start, false, true)?;
        declare_private_name(parser, &name, PrivateNameKind::Other, is_static)?;
        return Ok(vec![ClassElement::Method {
            decorators,
            is_static,
            name,
            function,
        }]);
    }
    // `async name() {}` / `async *name() {}`.
    if parser.at_contextual_unescaped("async")?
        && !parser.peek2()?.line_break_before
        && (is_class_name_start(parser.peek2()?.kind.clone())
            || matches!(parser.peek2()?.kind, TokenKind::Star))
    {
        let method_start = parser.peek()?.span.start; // `async`
        parser.next()?; // `async`
        let is_generator = parser.eat_punct(TokenKind::Star)?;
        let name = parse_class_element_name(parser)?;
        check_special_constructor(parser, &name, is_static)?;
        let function = parse_class_method_tail(parser, method_start, true, is_generator)?;
        declare_private_name(parser, &name, PrivateNameKind::Other, is_static)?;
        return Ok(vec![ClassElement::Method {
            decorators,
            is_static,
            name,
            function,
        }]);
    }
    // `get name() {}` / `set name(p) {}`.
    if parser.at_contextual_unescaped("get")? && is_class_name_start(parser.peek2()?.kind.clone()) {
        let accessor_start = parser.peek()?.span.start; // `get`
        parser.next()?; // `get`
        let name = parse_class_element_name(parser)?;
        check_special_constructor(parser, &name, is_static)?;
        parser.expect_punct(TokenKind::LeftParen)?;
        parser.expect_punct(TokenKind::RightParen)?;
        let (body, _) = parse_function_body_block(parser, false, false, &[], true, false, false)?;
        declare_private_name(parser, &name, PrivateNameKind::Getter(is_static), is_static)?;
        let span = Span::new(accessor_start, body.span.end);
        return Ok(vec![ClassElement::Get {
            decorators,
            is_static,
            name,
            body,
            span,
        }]);
    }
    if parser.at_contextual_unescaped("set")? && is_class_name_start(parser.peek2()?.kind.clone()) {
        let accessor_start = parser.peek()?.span.start; // `set`
        parser.next()?; // `set`
        let name = parse_class_element_name(parser)?;
        check_special_constructor(parser, &name, is_static)?;
        parser.expect_punct(TokenKind::LeftParen)?;
        // A setter takes a single FormalParameter, which may carry an
        // initializer (`set x(v = 1) {}`, spec 15.7.8).
        let element = parser.parse_binding_element()?;
        let param = element.pattern;
        let init = element.init;
        parser.expect_punct(TokenKind::RightParen)?;
        let (body, _) = parse_function_body_block(parser, false, false, &[], true, false, false)?;
        declare_private_name(parser, &name, PrivateNameKind::Setter(is_static), is_static)?;
        let span = Span::new(accessor_start, body.span.end);
        return Ok(vec![ClassElement::Set {
            decorators,
            is_static,
            name,
            param,
            init,
            body,
            span,
        }]);
    }

    // Plain method or field.
    let name_start = parser.peek()?.span.start;
    let name = parse_class_element_name(parser)?;
    if parser.at_punct(TokenKind::LeftParen)? {
        let in_constructor = !is_static && is_name(&name, "constructor");
        let function =
            parse_class_method_tail_with(parser, name_start, false, false, in_constructor)?;
        declare_private_name(parser, &name, PrivateNameKind::Other, is_static)?;
        return Ok(vec![ClassElement::Method {
            decorators,
            is_static,
            name,
            function,
        }]);
    }

    // Field: `name Initializer? ;`.
    declare_private_name(parser, &name, PrivateNameKind::Other, is_static)?;
    let init = parse_field_initializer(parser)?;
    parser.expect_semicolon()?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(vec![ClassElement::Field {
        decorators,
        is_static,
        name,
        init,
        span: Span::new(name_start, end),
    }])
}

/// `constructor` may not be a getter/setter/async/generator method.
fn check_special_constructor(
    parser: &mut Parser,
    name: &ClassElementName,
    is_static: bool,
) -> Result<(), JsError> {
    if !is_static && is_name(name, "constructor") {
        return Err(parser.error_at(
            parser.prev.as_ref().unwrap().span.start,
            "Class constructor may not be an accessor or special method",
        ));
    }
    Ok(())
}

/// `static { … }` — parsed as a strict, return-less statement list.
fn parse_static_block(parser: &mut Parser) -> Result<Block, JsError> {
    parser.expect_punct(TokenKind::LeftBrace)?;
    let start = parser.prev.as_ref().unwrap().span.start;
    let saved = (
        parser.strict,
        parser.in_function,
        parser.in_generator,
        parser.in_async,
        parser.allow_super,
        parser.in_constructor,
        parser.in_static_block,
        parser.nt_context,
    );
    parser.strict = true;
    parser.in_function = true;
    parser.in_generator = false;
    // Class static initialization blocks parse with an [Await] parameter
    // (spec 15.7.13), so `await` is reserved there.
    parser.in_async = true;
    parser.allow_super = true;
    parser.in_constructor = false;
    parser.in_static_block = true;
    // Static blocks are a function-like context for `new.target`.
    parser.nt_context = true;
    let saved_vars = std::mem::take(&mut parser.list_vars);
    parser.push_scope();
    let stmts = crate::stmt::parse_statement_list(parser, TokenKind::RightBrace)?;
    parser.expect_punct(TokenKind::RightBrace)?;
    let end = parser.prev.as_ref().unwrap().span.end;
    parser.pop_scope();
    parser.list_vars = saved_vars;
    (
        parser.strict,
        parser.in_function,
        parser.in_generator,
        parser.in_async,
        parser.allow_super,
        parser.in_constructor,
        parser.in_static_block,
        parser.nt_context,
    ) = saved;
    Ok(Block {
        stmts,
        span: Span::new(start, end),
    })
}

/// Whether a token can begin a class element name (property or private).
fn is_class_name_start(kind: TokenKind) -> bool {
    is_property_name_start(kind.clone()) || matches!(kind, TokenKind::PrivateIdentifier(_))
}

fn parse_class_element_name(parser: &mut Parser) -> Result<ClassElementName, JsError> {
    match parser.peek()?.kind.clone() {
        TokenKind::PrivateIdentifier(atom) => {
            parser.next()?;
            // Private identifiers are interned without the leading `#`.
            if atom == intern_utf8("constructor") {
                return Err(parser.error_at(
                    parser.prev.as_ref().unwrap().span.start,
                    "Private element may not be named #constructor",
                ));
            }
            Ok(ClassElementName::Private(atom))
        }
        _ => Ok(ClassElementName::Property(parser.parse_property_name()?)),
    }
}

/// Registers a private-name declaration, enforcing the duplicate rules
/// (a getter/setter pair is the only permitted double use).
fn declare_private_name(
    parser: &mut Parser,
    name: &ClassElementName,
    kind: PrivateNameKind,
    is_static: bool,
) -> Result<(), JsError> {
    let ClassElementName::Private(atom) = name else {
        return Ok(());
    };
    let map = parser.private_names.last_mut().unwrap();
    let entry = map.entry(*atom).or_default();
    let ok = match (*entry, kind, is_static) {
        (PrivateNameKind::None, k, s) => {
            *entry = k.with_static(s);
            true
        }
        (PrivateNameKind::Getter(g), PrivateNameKind::Setter(_), s) if g == s => {
            *entry = PrivateNameKind::GetterSetter {
                getter_static: g,
                setter_static: s,
            };
            true
        }
        (PrivateNameKind::Setter(g), PrivateNameKind::Getter(_), s) if g == s => {
            *entry = PrivateNameKind::GetterSetter {
                getter_static: s,
                setter_static: g,
            };
            true
        }
        _ => false,
    };
    if !ok {
        return Err(parser.error_at(
            parser.prev.as_ref().unwrap().span.start,
            "Duplicate private name",
        ));
    }
    Ok(())
}

/// Parses `( params ) { body }` for a class method, deciding whether the
/// method is the constructor from its name.
fn parse_class_method_tail(
    parser: &mut Parser,
    start: u32,
    is_async: bool,
    is_generator: bool,
) -> Result<Function, JsError> {
    parse_class_method_tail_with(parser, start, is_async, is_generator, false)
}

fn parse_class_method_tail_with(
    parser: &mut Parser,
    start: u32,
    is_async: bool,
    is_generator: bool,
    in_constructor: bool,
) -> Result<Function, JsError> {
    parser.expect_punct(TokenKind::LeftParen)?;
    // Params parse with the method's own [Yield, Await] grammar: an async
    // method's formal parameters reserve `await` (spec 15.8.1), and a plain
    // method's params reset both regardless of the enclosing context. `super`
    // is valid in a method's parameter list too (the initializers evaluate
    // with the method's home object, spec 15.7.5).
    let saved = (parser.in_generator, parser.in_async, parser.allow_super);
    parser.in_generator = is_generator;
    parser.in_async = is_async;
    parser.allow_super = true;
    let params = parse_parameter_list(parser)?;
    (parser.in_generator, parser.in_async, parser.allow_super) = saved;
    check_duplicate_params(parser, &params, false)?;
    crate::expr::check_function_params(parser, &params, is_async, is_generator)?;
    let (body, _) = parse_function_body_block(
        parser,
        is_async,
        is_generator,
        &params,
        true,
        in_constructor,
        false,
    )?;
    let end = body.span.end;
    Ok(Function {
        span: Span::new(start, end),
        name: None,
        params,
        body,
        is_async,
        is_generator,
        statement_position: false,
    })
}
