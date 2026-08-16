//! Class declarations and expressions (spec 15.7).

use crux::{JsError, Span, intern_utf8};
use syntax::keywords::Keyword;
use syntax::{Block, Class, ClassElement, ClassElementName, Function, TokenKind};

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
        let Some(element) = parse_class_element(parser)? else {
            continue; // `;` placeholder element
        };
        match &element {
            ClassElement::Method {
                is_static: false,
                name,
                function,
            } if is_plain_constructor(name, function) => {
                constructor_count += 1;
                if constructor_count > 1 {
                    return Err(
                        parser.error_at(element_start, "A class may only have one constructor")
                    );
                }
            }
            ClassElement::Field {
                is_static: false,
                name,
                ..
            } if is_name(name, "constructor") => {
                return Err(
                    parser.error_at(element_start, "Class field may not be named constructor")
                );
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
        elements.push(element);
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
        elements,
    })
}

/// Consumes a decorator list (`@expr @expr …`) before a class or class
/// element. The decorator grammar is a stage-3 proposal; each decorator is
/// validated syntactically and the results are discarded (no evaluation).
pub(crate) fn parse_decorators(parser: &mut Parser) -> Result<(), JsError> {
    while parser.eat_punct(TokenKind::At)? {
        if parser.at_punct(TokenKind::LeftParen)? {
            // `@( Expression )`
            parser.next()?;
            parse_expression(parser, true)?;
            parser.expect_punct(TokenKind::RightParen)?;
        } else {
            // `@ MemberExpression …` with optional `( args )`.
            let expr = parse_lhs(parser)?;
            if parser.at_punct(TokenKind::LeftParen)? {
                crate::expr::parse_arguments(parser)?;
            }
            let _ = expr;
        }
    }
    Ok(())
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

fn parse_class_element(parser: &mut Parser) -> Result<Option<ClassElement>, JsError> {
    if parser.eat_punct(TokenKind::Semicolon)? {
        return Ok(None);
    }
    // Decorators may precede any element (stage-3 proposal); they are
    // validated syntactically and discarded.
    if parser.at_punct(TokenKind::At)? {
        parse_decorators(parser)?;
    }
    let is_static = if parser.at_contextual_unescaped("static")? && static_is_prefix(parser)? {
        parser.next()?;
        true
    } else {
        false
    };

    // `static { … }` — a class static initialization block.
    if is_static && parser.at_punct(TokenKind::LeftBrace)? {
        let body = parse_static_block(parser)?;
        return Ok(Some(ClassElement::StaticBlock(body)));
    }

    // `accessor name …` — an auto-accessor field (decorators proposal); the
    // accessor semantics are not implemented, so the element parses as a
    // plain field.
    if parser.at_contextual_unescaped("accessor")? && accessor_is_prefix(parser)? {
        parser.next()?;
    }

    // `*name() {}` — generator method.
    if parser.eat_punct(TokenKind::Star)? {
        let method_start = parser.prev.as_ref().unwrap().span.start; // `*`
        let name = parse_class_element_name(parser)?;
        check_special_constructor(parser, &name, is_static)?;
        let function = parse_class_method_tail(parser, method_start, false, true)?;
        declare_private_name(parser, &name, PrivateNameKind::Other, is_static)?;
        return Ok(Some(ClassElement::Method {
            is_static,
            name,
            function,
        }));
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
        return Ok(Some(ClassElement::Method {
            is_static,
            name,
            function,
        }));
    }
    // `get name() {}` / `set name(p) {}`.
    if parser.at_contextual_unescaped("get")? && is_class_name_start(parser.peek2()?.kind.clone()) {
        parser.next()?; // `get`
        let name = parse_class_element_name(parser)?;
        check_special_constructor(parser, &name, is_static)?;
        parser.expect_punct(TokenKind::LeftParen)?;
        parser.expect_punct(TokenKind::RightParen)?;
        let (body, _) = parse_function_body_block(parser, false, false, &[], true, false, false)?;
        declare_private_name(parser, &name, PrivateNameKind::Getter(is_static), is_static)?;
        return Ok(Some(ClassElement::Get {
            is_static,
            name,
            body,
        }));
    }
    if parser.at_contextual_unescaped("set")? && is_class_name_start(parser.peek2()?.kind.clone()) {
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
        return Ok(Some(ClassElement::Set {
            is_static,
            name,
            param,
            init,
            body,
        }));
    }

    // Plain method or field.
    let name_start = parser.peek()?.span.start;
    let name = parse_class_element_name(parser)?;
    if parser.at_punct(TokenKind::LeftParen)? {
        let in_constructor = !is_static && is_name(&name, "constructor");
        let function =
            parse_class_method_tail_with(parser, name_start, false, false, in_constructor)?;
        declare_private_name(parser, &name, PrivateNameKind::Other, is_static)?;
        return Ok(Some(ClassElement::Method {
            is_static,
            name,
            function,
        }));
    }

    // Field: `name Initializer? ;` (an `accessor` field is still a field for
    // parsing purposes; the accessor semantics are not implemented).
    declare_private_name(parser, &name, PrivateNameKind::Other, is_static)?;
    let init = if parser.eat_punct(TokenKind::Equal)? {
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
        let value = parse_assignment(parser, true)?;
        (
            parser.allow_super,
            parser.in_constructor,
            parser.in_field_initializer,
        ) = saved;
        Some(value)
    } else {
        None
    };
    parser.expect_semicolon()?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Some(ClassElement::Field {
        is_static,
        name,
        init,
        span: Span::new(name_start, end),
    }))
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
