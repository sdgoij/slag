//! Expression parsing: the full precedence chain, cover grammar, arrows,
//! templates, and destructuring targets (spec ch. 13).

use crux::{AtomId, JsError, Span, intern_utf8};
use syntax::keywords::{Keyword, from_identifier, is_future_reserved_word};
use syntax::{
    Argument, ArrayBindingElement, ArrayElement, ArrayLiteral, ArrowBody, AssignOp, BinaryOp,
    BindingElement, BindingPattern, Block, CallExpr, Expr, ExprKind, Function, Literal, LogicalOp,
    MemberExpr, MemberProperty, NewExpr, ObjectBindingProperty, ObjectLiteral, ObjectProperty,
    PropertyName, TemplateElement, TemplateLiteral, TokenKind, UnaryOp, UpdateOp,
};

use crate::parser::{ParenItem, ParenResult, Parser};

/// Binary operator precedences (low to high). `??` is lowest and is barred
/// from mixing with `&&`/`||` (spec 13.13), and `**` is right-associative.
const PREC_NULLISH: u8 = 1;
const PREC_LOGICAL_OR: u8 = 2;
const PREC_LOGICAL_AND: u8 = 3;
const PREC_BIT_OR: u8 = 4;
const PREC_BIT_XOR: u8 = 5;
const PREC_BIT_AND: u8 = 6;
const PREC_EQUALITY: u8 = 7;
const PREC_RELATIONAL: u8 = 8;
const PREC_SHIFT: u8 = 9;
const PREC_ADDITIVE: u8 = 10;
const PREC_MULTIPLICATIVE: u8 = 11;
const PREC_EXPONENT: u8 = 12;

/// `Expression : AssignmentExpression , …` (spec 13.16).
pub(crate) fn parse_expression(parser: &mut Parser, allow_in: bool) -> Result<Expr, JsError> {
    let start = parser.peek()?.span.start;
    let mut exprs = vec![parse_assignment(parser, allow_in)?];
    while parser.eat_punct(TokenKind::Comma)? {
        exprs.push(parse_assignment(parser, allow_in)?);
    }
    if exprs.len() == 1 {
        return Ok(exprs.pop().unwrap());
    }
    let end = exprs.last().unwrap().span.end;
    Ok(Expr {
        span: Span::new(start, end),
        kind: ExprKind::Sequence(exprs),
    })
}

/// `AssignmentExpression` (spec 13.15): yield, async arrows, plain arrows,
/// conditional, and the assignment operators.
pub(crate) fn parse_assignment(parser: &mut Parser, allow_in: bool) -> Result<Expr, JsError> {
    let start = parser.peek()?.span.start;

    // Decorated class expressions: `@dec1 @dec2 class { … }`. The decorator
    // list is a stage-3 proposal; the expressions are validated syntactically
    // and not evaluated.
    if parser.at_punct(TokenKind::At)? {
        crate::class::parse_decorators(parser)?;
        parser.expect_keyword(Keyword::Class)?;
        let class_start = parser.prev.as_ref().unwrap().span.start;
        let class = crate::class::parse_class(parser, class_start, false)?;
        let span = class.span;
        let expr = Expr {
            span,
            kind: ExprKind::Class(Box::new(class)),
        };
        return parse_subscripts(parser, expr, false);
    }

    // `yield` in generators.
    if parser.in_generator && parser.at_contextual("yield")? {
        return parse_yield(parser, allow_in);
    }

    // `async function …`, `async x => …`, `async (…) => …` — `async` is
    // contextual, so only treat it as a header when the next token allows.
    // A call form (`async()`) falls through to the arrow/assignment handling
    // below so `async() = 1` parses as an assignment target (Annex B).
    let left: Expr = 'parsed: {
        if parser.at_contextual_unescaped("async")? && !parser.peek2()?.line_break_before {
            let next_kind = parser.peek2()?.kind.clone();
            match next_kind {
                TokenKind::Identifier(atom) if from_identifier(atom) == Some(Keyword::Function) => {
                    parser.next()?; // `async`
                    // FunctionExpression is a PrimaryExpression, so the member/
                    // call chain continues (`async function () {}.constructor`).
                    let function = parse_function_expression(parser, true)?;
                    return parse_subscripts(parser, function, false);
                }
                TokenKind::Identifier(atom) if is_binding_identifier(parser, atom) => {
                    // `async x => …` — the single parameter is a
                    // `BindingIdentifier[~Yield, +Await]` (spec 15.8.1). Only
                    // commit when an arrow follows: `async of [7]` in a
                    // for-await-of head is an identifier, not an arrow.
                    if parser.peek3()?.kind != TokenKind::Arrow {
                        break 'parsed parse_conditional(parser, allow_in)?;
                    }
                    parser.next()?; // `async`
                    let saved_async = parser.in_async;
                    parser.in_async = true;
                    let params = parse_single_arrow_param(parser)?;
                    parser.in_async = saved_async;
                    check_arrow_params(parser, &params)?;
                    parser.expect_punct(TokenKind::Arrow)?;
                    let body = parse_arrow_body(parser, true, &params)?;
                    let end = parser.prev.as_ref().unwrap().span.end;
                    return Ok(Expr {
                        span: Span::new(start, end),
                        kind: ExprKind::Arrow {
                            is_async: true,
                            params,
                            body,
                        },
                    });
                }
                TokenKind::LeftParen => {
                    // `async (…) => …` or `async(…)` call. The parameter list
                    // parses with [+Await] (spec 15.8.1).
                    parser.next()?; // `async`
                    parser.expect_punct(TokenKind::LeftParen)?;
                    let saved_async = parser.in_async;
                    parser.in_async = true;
                    let result = parse_paren_contents(parser)?;
                    parser.in_async = saved_async;
                    match result {
                        ParenResult::ArrowParams(params) => {
                            parser.expect_punct(TokenKind::Arrow)?;
                            let body = parse_arrow_body(parser, true, &params)?;
                            let end = parser.prev.as_ref().unwrap().span.end;
                            return Ok(Expr {
                                span: Span::new(start, end),
                                kind: ExprKind::Arrow {
                                    is_async: true,
                                    params,
                                    body,
                                },
                            });
                        }
                        ParenResult::Empty => {
                            let end = parser.prev.as_ref().unwrap().span.end;
                            break 'parsed Expr {
                                span: Span::new(start, end),
                                kind: ExprKind::Call(CallExpr {
                                    callee: Box::new(Expr {
                                        span: Span::new(start, start),
                                        kind: ExprKind::Ident(intern_utf8("async")),
                                    }),
                                    args: Vec::new(),
                                    optional: false,
                                    span: Span::new(start, end),
                                }),
                            };
                        }
                        ParenResult::Expr(inner) => {
                            // `async(expr)` — rebuild the call from the paren list.
                            let end = parser.prev.as_ref().unwrap().span.end;
                            let args = split_sequence_into_args(inner);
                            break 'parsed Expr {
                                span: Span::new(start, end),
                                kind: ExprKind::Call(CallExpr {
                                    callee: Box::new(Expr {
                                        span: Span::new(start, start),
                                        kind: ExprKind::Ident(intern_utf8("async")),
                                    }),
                                    args,
                                    optional: false,
                                    span: Span::new(start, end),
                                }),
                            };
                        }
                    }
                }
                _ => {}
            }
        }
        parse_conditional(parser, allow_in)?
    };

    // Postfix `++`/`--` on the async-call form (`async()++`): the async
    // special-case above skipped parse_conditional, which normally consumes
    // the update operator.
    if parser.at_punct(TokenKind::PlusPlus)? && !parser.peek()?.line_break_before {
        parser.check_update_target(&left)?;
        let end = parser.peek()?.span.end;
        parser.next()?;
        return Ok(Expr {
            span: Span::new(left.span.start, end),
            kind: ExprKind::Update {
                op: UpdateOp::Increment,
                prefix: false,
                target: Box::new(left),
            },
        });
    }
    if parser.at_punct(TokenKind::MinusMinus)? && !parser.peek()?.line_break_before {
        parser.check_update_target(&left)?;
        let end = parser.peek()?.span.end;
        parser.next()?;
        return Ok(Expr {
            span: Span::new(left.span.start, end),
            kind: ExprKind::Update {
                op: UpdateOp::Decrement,
                prefix: false,
                target: Box::new(left),
            },
        });
    }

    // `x => body` — single-parameter arrow.
    if parser.at_punct(TokenKind::Arrow)? && !parser.peek()?.line_break_before {
        let ExprKind::Ident(name) = left.kind else {
            return Err(parser.error_at(left.span.start, "Invalid arrow-function parameter list"));
        };
        let params = vec![BindingElement {
            pattern: BindingPattern::Ident(name),
            init: None,
            rest: false,
            span: left.span,
        }];
        check_arrow_params(parser, &params)?;
        parser.next()?; // `=>`
        let body = parse_arrow_body(parser, false, &params)?;
        let end = parser.prev.as_ref().unwrap().span.end;
        return Ok(Expr {
            span: Span::new(start, end),
            kind: ExprKind::Arrow {
                is_async: false,
                params,
                body,
            },
        });
    }

    // Assignment operators (right-associative).
    if let Some(op) = assignment_op(parser.peek()?.kind.clone()) {
        parser.check_assignment_target(&left, op)?;
        parser.next()?;
        // A destructuring target absorbs any pending cover error from
        // `{a = 1}` inside the target.
        if op == AssignOp::Assign && matches!(left.kind, ExprKind::Array(_) | ExprKind::Object(_)) {
            parser.cover_error = None;
        }
        let value = parse_assignment(parser, allow_in)?;
        let end = value.span.end;
        return Ok(Expr {
            span: Span::new(start, end),
            kind: ExprKind::Assign {
                op,
                target: Box::new(left),
                value: Box::new(value),
            },
        });
    }

    // `left` is a plain expression: a deferred CoverInitializedName that was
    // not absorbed by an enclosing pattern is an error (spec 13.2.5.1). The
    // raise is skipped while parsing a pattern candidate (an array/object
    // element or a for-head), whose enclosing literal decides instead.
    if parser.suppress_cover_raise == 0
        && let Some(span) = parser.cover_error
    {
        return Err(parser.error_at(span.start, "Invalid shorthand property initializer"));
    }

    Ok(left)
}

fn is_binding_identifier(parser: &Parser, atom: AtomId) -> bool {
    if from_identifier(atom).is_some() {
        return false;
    }
    if parser.strict && syntax::keywords::is_future_reserved_word(atom) {
        return false;
    }
    if parser.in_generator && atom == intern_utf8("yield") {
        return false;
    }
    if (parser.in_async || parser.in_module) && atom == intern_utf8("await") {
        return false;
    }
    true
}

/// `async x => …`: parse the single binding identifier.
fn parse_single_arrow_param(parser: &mut Parser) -> Result<Vec<BindingElement>, JsError> {
    let tok = parser.peek()?.clone();
    let TokenKind::Identifier(_) = tok.kind else {
        return Err(parser.unexpected(&tok));
    };
    let (name, start) = parser.parse_identifier()?;
    Ok(vec![BindingElement {
        pattern: BindingPattern::Ident(name),
        init: None,
        rest: false,
        span: Span::new(start, tok.span.end),
    }])
}

/// Splits a parenthesized expression back into call arguments.
fn split_sequence_into_args(expr: Expr) -> Vec<Argument> {
    match expr.kind {
        ExprKind::Sequence(parts) => parts.into_iter().map(Argument::Expr).collect(),
        other => vec![Argument::Expr(Expr {
            span: expr.span,
            kind: other,
        })],
    }
}

fn parse_yield(parser: &mut Parser, allow_in: bool) -> Result<Expr, JsError> {
    let start = parser.next()?.span.start; // `yield`
    if parser.peek()?.line_break_before {
        return Ok(Expr {
            span: Span::new(start, start),
            kind: ExprKind::Yield {
                delegate: false,
                argument: None,
            },
        });
    }
    if parser.eat_punct(TokenKind::Star)? {
        // The `*` operand is an AssignmentExpression[?In, +Yield]: the [~In]
        // of a for-head propagates, so `for (yield * x in y; …)` is an error.
        let argument = parse_assignment(parser, allow_in)?;
        let end = argument.span.end;
        return Ok(Expr {
            span: Span::new(start, end),
            kind: ExprKind::Yield {
                delegate: true,
                argument: Some(Box::new(argument)),
            },
        });
    }
    if can_start_expression(parser.peek()?.kind.clone()) {
        let argument = parse_assignment(parser, allow_in)?;
        let end = argument.span.end;
        return Ok(Expr {
            span: Span::new(start, end),
            kind: ExprKind::Yield {
                delegate: false,
                argument: Some(Box::new(argument)),
            },
        });
    }
    Ok(Expr {
        span: Span::new(start, start),
        kind: ExprKind::Yield {
            delegate: false,
            argument: None,
        },
    })
}

/// Whether a token can begin an expression (used after `yield`/`return`/
/// `throw`).
pub(crate) fn can_start_expression(kind: TokenKind) -> bool {
    match kind {
        TokenKind::Identifier(atom) => match from_identifier(atom) {
            None => true,
            Some(k) => matches!(
                k,
                Keyword::This
                    | Keyword::Super
                    | Keyword::Function
                    | Keyword::Class
                    | Keyword::New
                    | Keyword::Delete
                    | Keyword::Void
                    | Keyword::Typeof
                    | Keyword::Import
                    | Keyword::True
                    | Keyword::False
                    | Keyword::Null
            ),
        },
        TokenKind::NullLiteral
        | TokenKind::BooleanLiteral(_)
        | TokenKind::NumericLiteral(_)
        | TokenKind::StringLiteral { .. }
        | TokenKind::RegExpLiteral { .. }
        | TokenKind::NoSubstitutionTemplate { .. }
        | TokenKind::TemplateHead { .. }
        | TokenKind::LeftParen
        | TokenKind::LeftBracket
        | TokenKind::LeftBrace
        | TokenKind::PrivateIdentifier(_) => true,
        TokenKind::Plus
        | TokenKind::Minus
        | TokenKind::Tilde
        | TokenKind::Not
        | TokenKind::PlusPlus
        | TokenKind::MinusMinus => true,
        _ => false,
    }
}

/// `ConditionalExpression` (spec 13.14).
fn parse_conditional(parser: &mut Parser, allow_in: bool) -> Result<Expr, JsError> {
    let start = parser.peek()?.span.start;
    let test = parse_short_circuit(parser, allow_in)?;
    if parser.eat_punct(TokenKind::Question)? {
        let consequent = parse_assignment(parser, true)?; // [+In]
        parser.expect_punct(TokenKind::Colon)?;
        let alternate = parse_assignment(parser, allow_in)?;
        let end = alternate.span.end;
        return Ok(Expr {
            span: Span::new(start, end),
            kind: ExprKind::Conditional {
                test: Box::new(test),
                consequent: Box::new(consequent),
                alternate: Box::new(alternate),
            },
        });
    }
    Ok(test)
}

fn parse_short_circuit(parser: &mut Parser, allow_in: bool) -> Result<Expr, JsError> {
    parse_binary(parser, allow_in, 0)
}

/// Precedence-climbing binary parser (spec 13.6-13.13).
fn parse_binary(parser: &mut Parser, allow_in: bool, min_prec: u8) -> Result<Expr, JsError> {
    let mut left = if matches!(parser.peek()?.kind, TokenKind::PrivateIdentifier(_)) {
        // `#name in object` (spec 13.11): a PrivateIdentifier is only valid
        // as the left operand of `in`.
        let start = parser.peek()?.span.start;
        let TokenKind::PrivateIdentifier(atom) = parser.next()?.kind else {
            unreachable!("peeked a private identifier")
        };
        // AllPrivateIdentifiersValid (spec 13.11): a PrivateIdentifier needs
        // an enclosing class that declares it.
        if parser.private_names.is_empty() {
            return Err(parser.error_at(start, "Private field access is only valid inside a class"));
        }
        // The production is `PrivateIdentifier in ShiftExpression`, so the
        // `in` must bind at this precedence level. A bare PrivateIdentifier
        // is not an expression; this also rejects `#a in #b in this`, whose
        // right operand would have to be a ShiftExpression.
        if min_prec > PREC_RELATIONAL || !allow_in || !parser.at_keyword(Keyword::In)? {
            return Err(parser.error_at(
                start,
                "A private identifier is only valid as the left operand of 'in'",
            ));
        }
        parser.next()?; // `in`
        // The right operand is a ShiftExpression: it may not contain `in`,
        // and an arrow function (an AssignmentExpression) at this level is
        // never a ShiftExpression (spec 13.11).
        let saved_arrow = parser.reject_arrow;
        parser.reject_arrow = true;
        let right = parse_binary(parser, false, PREC_SHIFT)?;
        parser.reject_arrow = saved_arrow;
        let end = right.span.end;
        Expr {
            span: Span::new(start, end),
            kind: ExprKind::PrivateIn {
                name: atom,
                object: Box::new(right),
            },
        }
    } else {
        parse_unary(parser)?
    };
    loop {
        let kind = parser.peek()?.kind.clone();
        let logical = logical_kind(kind.clone());
        let prec = if let Some(lop) = logical {
            Some(match lop {
                LogicalOp::Nullish => PREC_NULLISH,
                LogicalOp::Or => PREC_LOGICAL_OR,
                LogicalOp::And => PREC_LOGICAL_AND,
            })
        } else {
            binary_operator(parser, allow_in)?.map(|(p, _)| p)
        };
        let Some(prec) = prec else {
            break;
        };
        if prec < min_prec {
            break;
        }
        // The 17th-edition grammar forbids mixing `??` with `&&`/`||`
        // without parentheses; detect it structurally.
        if let Some(logical_op) = logical {
            if matches!(logical_op, LogicalOp::Nullish) {
                if matches!(
                    left.kind,
                    ExprKind::Logical {
                        op: LogicalOp::And | LogicalOp::Or,
                        ..
                    }
                ) {
                    return Err(parser.error_at(
                        left.span.start,
                        "Nullish coalescing operator cannot mix with logical operators without parentheses",
                    ));
                }
            } else if matches!(
                left.kind,
                ExprKind::Logical {
                    op: LogicalOp::Nullish,
                    ..
                }
            ) {
                return Err(parser.error_at(
                    left.span.start,
                    "Nullish coalescing operator cannot mix with logical operators without parentheses",
                ));
            }
        }
        let op = if let Some(lop) = logical {
            // Logical operators build a Logical node; the operator value is
            // unused here.
            match lop {
                LogicalOp::Nullish => BinaryOp::Equal,
                LogicalOp::Or => BinaryOp::Equal,
                LogicalOp::And => BinaryOp::Equal,
            }
        } else {
            binary_operator(parser, allow_in)?.map(|(_, o)| o).unwrap()
        };
        parser.next()?;
        let right_min = if matches!(kind, TokenKind::NullishCoalescing) {
            PREC_BIT_OR // the `??` right operand is a BitwiseORExpression
        } else if matches!(kind, TokenKind::StarStar) {
            PREC_EXPONENT // right-associative
        } else {
            prec + 1
        };
        if matches!(kind, TokenKind::StarStar)
            && matches!(left.kind, ExprKind::Unary { .. } | ExprKind::Await(_))
        {
            return Err(parser.error_at(
                left.span.start,
                "Unary expression cannot be the left operand of **",
            ));
        }
        let right = parse_binary(parser, allow_in, right_min)?;
        let span = Span::new(left.span.start, right.span.end);
        left = if let Some(logical_op) = logical {
            Expr {
                span,
                kind: ExprKind::Logical {
                    op: logical_op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
            }
        } else {
            Expr {
                span,
                kind: ExprKind::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
            }
        };
    }
    Ok(left)
}

/// Maps a token to its binary precedence. The operator kind is derived by the
/// caller; keywords (`in`, `instanceof`) are handled via `at_keyword`.
fn binary_operator(parser: &mut Parser, allow_in: bool) -> Result<Option<(u8, BinaryOp)>, JsError> {
    let kind = parser.peek()?.kind.clone();
    let (prec, op): (u8, BinaryOp) = match kind {
        TokenKind::Pipe => (PREC_BIT_OR, BinaryOp::BitOr),
        TokenKind::Caret => (PREC_BIT_XOR, BinaryOp::BitXor),
        TokenKind::Ampersand => (PREC_BIT_AND, BinaryOp::BitAnd),
        TokenKind::EqualEqual => (PREC_EQUALITY, BinaryOp::Equal),
        TokenKind::NotEqual => (PREC_EQUALITY, BinaryOp::NotEqual),
        TokenKind::StrictEqual => (PREC_EQUALITY, BinaryOp::StrictEqual),
        TokenKind::StrictNotEqual => (PREC_EQUALITY, BinaryOp::StrictNotEqual),
        TokenKind::LessThan => (PREC_RELATIONAL, BinaryOp::LessThan),
        TokenKind::GreaterThan => (PREC_RELATIONAL, BinaryOp::GreaterThan),
        TokenKind::LessEqual => (PREC_RELATIONAL, BinaryOp::LessEqual),
        TokenKind::GreaterEqual => (PREC_RELATIONAL, BinaryOp::GreaterEqual),
        TokenKind::LeftShift => (PREC_SHIFT, BinaryOp::LeftShift),
        TokenKind::RightShift => (PREC_SHIFT, BinaryOp::RightShift),
        TokenKind::UnsignedRightShift => (PREC_SHIFT, BinaryOp::UnsignedRightShift),
        TokenKind::Plus => (PREC_ADDITIVE, BinaryOp::Add),
        TokenKind::Minus => (PREC_ADDITIVE, BinaryOp::Sub),
        TokenKind::Star => (PREC_MULTIPLICATIVE, BinaryOp::Mul),
        TokenKind::Slash => (PREC_MULTIPLICATIVE, BinaryOp::Div),
        TokenKind::Percent => (PREC_MULTIPLICATIVE, BinaryOp::Rem),
        TokenKind::StarStar => (PREC_EXPONENT, BinaryOp::Exp),
        TokenKind::Identifier(atom) => match from_identifier(atom) {
            Some(Keyword::Instanceof) => (PREC_RELATIONAL, BinaryOp::Instanceof),
            Some(Keyword::In) if allow_in => (PREC_RELATIONAL, BinaryOp::In),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    Ok(Some((prec, op)))
}

/// The kind of a logical operator token, if any.
fn logical_kind(kind: TokenKind) -> Option<LogicalOp> {
    match kind {
        TokenKind::NullishCoalescing => Some(LogicalOp::Nullish),
        TokenKind::Or => Some(LogicalOp::Or),
        TokenKind::And => Some(LogicalOp::And),
        _ => None,
    }
}

/// Maps an assignment token to its operator.
fn assignment_op(kind: TokenKind) -> Option<AssignOp> {
    Some(match kind {
        TokenKind::Equal => AssignOp::Assign,
        TokenKind::PlusEqual => AssignOp::AddAssign,
        TokenKind::MinusEqual => AssignOp::SubAssign,
        TokenKind::StarEqual => AssignOp::MulAssign,
        TokenKind::SlashEqual => AssignOp::DivAssign,
        TokenKind::PercentEqual => AssignOp::RemAssign,
        TokenKind::StarStarEqual => AssignOp::ExpAssign,
        TokenKind::LeftShiftEqual => AssignOp::LeftShiftAssign,
        TokenKind::RightShiftEqual => AssignOp::RightShiftAssign,
        TokenKind::UnsignedRightShiftEqual => AssignOp::UnsignedRightShiftAssign,
        TokenKind::AmpersandEqual => AssignOp::BitAndAssign,
        TokenKind::CaretEqual => AssignOp::BitXorAssign,
        TokenKind::PipeEqual => AssignOp::BitOrAssign,
        TokenKind::AndEqual => AssignOp::AndAssign,
        TokenKind::OrEqual => AssignOp::OrAssign,
        TokenKind::NullishCoalescingEqual => AssignOp::NullishAssign,
        _ => return None,
    })
}

/// `UnaryExpression` (spec 13.6.2): prefix update, unary operators, await.
fn parse_unary(parser: &mut Parser) -> Result<Expr, JsError> {
    let start = parser.peek()?.span.start;
    if parser.at_punct(TokenKind::PlusPlus)? || parser.at_punct(TokenKind::MinusMinus)? {
        let op = if parser.at_punct(TokenKind::PlusPlus)? {
            UpdateOp::Increment
        } else {
            UpdateOp::Decrement
        };
        parser.next()?;
        let operand = parse_unary(parser)?;
        // `++x` requires x to be a valid update target (spec 13.5.2); this
        // also rejects `++import('')`, `++a?.b`, and `++(++y)`.
        parser.check_update_target(&operand)?;
        let end = operand.span.end;
        return Ok(Expr {
            span: Span::new(start, end),
            kind: ExprKind::Update {
                op,
                prefix: true,
                target: Box::new(operand),
            },
        });
    }
    if (parser.in_async || parser.top_level_await) && parser.at_contextual("await")? {
        parser.next()?;
        let operand = parse_unary(parser)?;
        let end = operand.span.end;
        return Ok(Expr {
            span: Span::new(start, end),
            kind: ExprKind::Await(Box::new(operand)),
        });
    }
    let unary = if parser.at_keyword(Keyword::Delete)? {
        Some(UnaryOp::Delete)
    } else if parser.at_keyword(Keyword::Void)? {
        Some(UnaryOp::Void)
    } else if parser.at_keyword(Keyword::Typeof)? {
        Some(UnaryOp::Typeof)
    } else {
        match parser.peek()?.kind.clone() {
            TokenKind::Plus => Some(UnaryOp::Plus),
            TokenKind::Minus => Some(UnaryOp::Minus),
            TokenKind::Tilde => Some(UnaryOp::BitNot),
            TokenKind::Not => Some(UnaryOp::Not),
            _ => None,
        }
    };
    if let Some(op) = unary {
        parser.next()?;
        let operand = parse_unary(parser)?;
        if op == UnaryOp::Delete {
            // Parentheses do not hide the derived UnaryExpression for the
            // delete early errors (spec 13.6.2).
            let mut derived = &operand;
            while let ExprKind::Paren(inner) = &derived.kind {
                derived = inner;
            }
            if parser.strict && matches!(derived.kind, ExprKind::Ident(_)) {
                return Err(parser.error_at(
                    start,
                    "Deleting an unqualified identifier is not allowed in strict mode",
                ));
            }
            // `delete x.#p` — a MemberExpression.PrivateName operand is an
            // early error in strict mode (class bodies are always strict).
            if matches!(
                derived.kind,
                ExprKind::Member(syntax::MemberExpr {
                    property: MemberProperty::Private(_),
                    ..
                })
            ) {
                return Err(
                    parser.error_at(start, "Delete of a private field or method is not allowed")
                );
            }
        }
        let end = operand.span.end;
        return Ok(Expr {
            span: Span::new(start, end),
            kind: ExprKind::Unary {
                op,
                operand: Box::new(operand),
            },
        });
    }
    let expr = parse_update(parser)?;
    Ok(expr)
}

/// `UpdateExpression` (spec 13.5): left-hand side with postfix `++`/`--`.
fn parse_update(parser: &mut Parser) -> Result<Expr, JsError> {
    let expr = parse_lhs(parser)?;
    if parser.at_punct(TokenKind::PlusPlus)? && !parser.peek()?.line_break_before {
        parser.check_update_target(&expr)?;
        let end = parser.peek()?.span.end;
        parser.next()?;
        return Ok(Expr {
            span: Span::new(expr.span.start, end),
            kind: ExprKind::Update {
                op: UpdateOp::Increment,
                prefix: false,
                target: Box::new(expr),
            },
        });
    }
    if parser.at_punct(TokenKind::MinusMinus)? && !parser.peek()?.line_break_before {
        parser.check_update_target(&expr)?;
        let end = parser.peek()?.span.end;
        parser.next()?;
        return Ok(Expr {
            span: Span::new(expr.span.start, end),
            kind: ExprKind::Update {
                op: UpdateOp::Decrement,
                prefix: false,
                target: Box::new(expr),
            },
        });
    }
    Ok(expr)
}

/// `LeftHandSideExpression`: `new`, `super`, member/call chains, optional
/// chains (spec 13.4).
pub(crate) fn parse_lhs(parser: &mut Parser) -> Result<Expr, JsError> {
    if parser.at_keyword(Keyword::New)? {
        // `new MemberExpression Arguments` is itself a MemberExpression, so
        // the member/call chain continues after it (`new C(5).x`).
        let expr = parse_new(parser)?;
        return parse_subscripts(parser, expr, false);
    }
    if parser.at_keyword(Keyword::Super)? {
        let expr = parse_super(parser)?;
        // `super.m()` is a CallExpression whose callee is the super member
        // access, so the chain continues with subscripts.
        return parse_subscripts(parser, expr, false);
    }
    let expr = parse_primary(parser)?;
    parse_subscripts(parser, expr, false)
}

fn parse_new(parser: &mut Parser) -> Result<Expr, JsError> {
    let start = parser.next()?.span.start; // `new`
    if parser.eat_punct(TokenKind::Dot)? {
        // `new . target` — the MetaProperty requires the exact code units
        // `target`; an escaped form is not a MetaProperty (spec 13.3.4).
        if parser.peek()?.escaped {
            return Err(parser.error_at(start, "Expected new.target"));
        }
        let (atom, _) = parser.parse_identifier()?;
        if atom != intern_utf8("target") {
            return Err(parser.error_at(start, "Expected new.target"));
        }
        // `new.target` is an early error outside functions (spec 13.3.4,
        // 15.2.2) except inside class field initializers and static blocks.
        if !parser.nt_context && !parser.in_field_initializer {
            return Err(parser.error_at(start, "new.target is not allowed here"));
        }
        let end = parser.prev.as_ref().unwrap().span.end;
        return Ok(Expr {
            span: Span::new(start, end),
            kind: ExprKind::MetaProperty {
                meta: intern_utf8("new"),
                property: atom,
            },
        });
    }
    // Callee: a member chain that must not consume call arguments (the `(`
    // belongs to this `new`).
    let callee = if parser.at_keyword(Keyword::New)? {
        parse_new(parser)?
    } else {
        let base = parse_primary(parser)?;
        parse_subscripts(parser, base, true)?
    };
    if contains_optional(&callee) {
        return Err(parser.error_at(
            callee.span.start,
            "Optional chaining cannot appear in a new expression",
        ));
    }
    // ImportCall is a CallExpression, never a MemberExpression, so it cannot
    // be the callee of `new` (spec 13.3.4).
    if import_call_base(&callee) {
        return Err(parser.error_at(callee.span.start, "import() cannot be used with new"));
    }
    let args = if parser.at_punct(TokenKind::LeftParen)? {
        parse_arguments(parser)?
    } else {
        Vec::new()
    };
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Expr {
        span: Span::new(start, end),
        kind: ExprKind::New(NewExpr {
            callee: Box::new(callee),
            args,
            span: Span::new(start, end),
        }),
    })
}

fn parse_super(parser: &mut Parser) -> Result<Expr, JsError> {
    let start = parser.next()?.span.start; // `super`
    if !parser.allow_super {
        return Err(parser.error_at(start, "super is only valid inside class methods"));
    }
    let super_expr = Expr {
        span: Span::new(start, start),
        kind: ExprKind::Super,
    };
    match parser.peek()?.kind.clone() {
        TokenKind::Dot => {
            parser.next()?;
            // `super.#p` is not a valid SuperProperty (spec 15.7.10).
            if matches!(parser.peek()?.kind, TokenKind::PrivateIdentifier(_)) {
                let tok = parser.peek()?.clone();
                return Err(parser.error_at(
                    tok.span.start,
                    "super property may not be a private identifier",
                ));
            }
            let property = parse_member_property(parser)?;
            let end = parser.prev.as_ref().unwrap().span.end;
            Ok(Expr {
                span: Span::new(start, end),
                kind: ExprKind::Member(MemberExpr {
                    object: Box::new(super_expr),
                    property,
                    optional: false,
                    span: Span::new(start, end),
                }),
            })
        }
        TokenKind::LeftBracket => {
            parser.next()?;
            let index = parse_expression(parser, true)?;
            parser.expect_punct(TokenKind::RightBracket)?;
            let end = parser.prev.as_ref().unwrap().span.end;
            Ok(Expr {
                span: Span::new(start, end),
                kind: ExprKind::Member(MemberExpr {
                    object: Box::new(super_expr),
                    property: MemberProperty::Computed(Box::new(index)),
                    optional: false,
                    span: Span::new(start, end),
                }),
            })
        }
        TokenKind::LeftParen => {
            // `super()` requires a constructor of a derived class (spec
            // 15.7.11 early errors: no heritage and HasDirectSuper).
            if !parser.in_constructor || !parser.in_derived_class {
                return Err(parser.error_at(
                    start,
                    "super() is only valid inside the constructor of a derived class",
                ));
            }
            let args = parse_arguments(parser)?;
            let end = parser.prev.as_ref().unwrap().span.end;
            Ok(Expr {
                span: Span::new(start, end),
                kind: ExprKind::Call(CallExpr {
                    callee: Box::new(super_expr),
                    args,
                    optional: false,
                    span: Span::new(start, end),
                }),
            })
        }
        _ => Err(parser.error_at(start, "super must be followed by an access or call")),
    }
}

/// Parses the `.name`, `.#private`, or `[expr]` member forms.
fn parse_member_property(parser: &mut Parser) -> Result<MemberProperty, JsError> {
    match parser.peek()?.kind.clone() {
        TokenKind::Identifier(atom) => {
            parser.next()?;
            Ok(MemberProperty::Name(atom))
        }
        TokenKind::PrivateIdentifier(atom) => {
            // AllPrivateIdentifiersValid (spec 13.4): a PrivateIdentifier
            // needs an enclosing class that declares it.
            if parser.private_names.is_empty() {
                let at = parser.peek()?.span.start;
                return Err(
                    parser.error_at(at, "Private field access is only valid inside a class")
                );
            }
            parser.next()?;
            Ok(MemberProperty::Private(atom))
        }
        _ => {
            let tok = parser.peek()?.clone();
            Err(parser.unexpected(&tok))
        }
    }
}

/// Member/call/optional-chain continuation (spec 13.4.1).
pub(crate) fn parse_subscripts(
    parser: &mut Parser,
    mut expr: Expr,
    no_calls: bool,
) -> Result<Expr, JsError> {
    loop {
        match parser.peek()?.kind.clone() {
            TokenKind::Dot => {
                parser.next()?;
                let property = parse_member_property(parser)?;
                let end = parser.prev.as_ref().unwrap().span.end;
                let start = expr.span.start;
                expr = Expr {
                    span: Span::new(start, end),
                    kind: ExprKind::Member(MemberExpr {
                        object: Box::new(expr),
                        property,
                        optional: false,
                        span: Span::new(start, end),
                    }),
                };
            }
            TokenKind::LeftBracket => {
                parser.next()?;
                let index = parse_expression(parser, true)?;
                parser.expect_punct(TokenKind::RightBracket)?;
                let end = parser.prev.as_ref().unwrap().span.end;
                let start = expr.span.start;
                expr = Expr {
                    span: Span::new(start, end),
                    kind: ExprKind::Member(MemberExpr {
                        object: Box::new(expr),
                        property: MemberProperty::Computed(Box::new(index)),
                        optional: false,
                        span: Span::new(start, end),
                    }),
                };
            }
            TokenKind::LeftParen if !no_calls => {
                let args = parse_arguments(parser)?;
                let end = parser.prev.as_ref().unwrap().span.end;
                let start = expr.span.start;
                expr = Expr {
                    span: Span::new(start, end),
                    kind: ExprKind::Call(CallExpr {
                        callee: Box::new(expr),
                        args,
                        optional: false,
                        span: Span::new(start, end),
                    }),
                };
            }
            TokenKind::QuestionDot => {
                parser.next()?;
                expr = parse_optional_link(parser, expr)?;
            }
            TokenKind::NoSubstitutionTemplate { .. } | TokenKind::TemplateHead { .. } => {
                expr = parse_tagged_template(parser, expr)?;
            }
            _ => break,
        }
    }
    Ok(expr)
}

/// Parses one `?.` link: `?.name`, `?.#private`, `?.[expr]`, `?.(args)`,
/// `?.`template.
fn parse_optional_link(parser: &mut Parser, expr: Expr) -> Result<Expr, JsError> {
    let start = expr.span.start;
    match parser.peek()?.kind.clone() {
        TokenKind::Identifier(atom) => {
            parser.next()?;
            let end = parser.prev.as_ref().unwrap().span.end;
            Ok(Expr {
                span: Span::new(start, end),
                kind: ExprKind::Member(MemberExpr {
                    object: Box::new(expr),
                    property: MemberProperty::Name(atom),
                    optional: true,
                    span: Span::new(start, end),
                }),
            })
        }
        TokenKind::PrivateIdentifier(atom) => {
            parser.next()?;
            let end = parser.prev.as_ref().unwrap().span.end;
            Ok(Expr {
                span: Span::new(start, end),
                kind: ExprKind::Member(MemberExpr {
                    object: Box::new(expr),
                    property: MemberProperty::Private(atom),
                    optional: true,
                    span: Span::new(start, end),
                }),
            })
        }
        TokenKind::LeftBracket => {
            parser.next()?;
            let index = parse_expression(parser, true)?;
            parser.expect_punct(TokenKind::RightBracket)?;
            let end = parser.prev.as_ref().unwrap().span.end;
            Ok(Expr {
                span: Span::new(start, end),
                kind: ExprKind::Member(MemberExpr {
                    object: Box::new(expr),
                    property: MemberProperty::Computed(Box::new(index)),
                    optional: true,
                    span: Span::new(start, end),
                }),
            })
        }
        TokenKind::LeftParen => {
            let args = parse_arguments(parser)?;
            let end = parser.prev.as_ref().unwrap().span.end;
            Ok(Expr {
                span: Span::new(start, end),
                kind: ExprKind::Call(CallExpr {
                    callee: Box::new(expr),
                    args,
                    optional: true,
                    span: Span::new(start, end),
                }),
            })
        }
        // A template after `?.` is never valid: the OptionalChain grammar has
        // no template link (spec 13.4.1), so `a?.`x`` is an early error even
        // across a line break.
        TokenKind::NoSubstitutionTemplate { .. } | TokenKind::TemplateHead { .. } => {
            Err(parser.error_at(start, "Template literals are not allowed after ?."))
        }
        _ => {
            let tok = parser.peek()?.clone();
            Err(parser.unexpected(&tok))
        }
    }
}

/// Whether an expression contains an optional-chain link (spec 13.4.4 early
/// errors: no `new`, no assignment targets, no tagged templates on chains).
pub(crate) fn contains_optional(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Member(m) => m.optional || contains_optional(&m.object),
        ExprKind::Call(c) => c.optional || contains_optional(&c.callee),
        ExprKind::TaggedTemplate { tag, .. } => contains_optional(tag),
        ExprKind::Paren(inner) => contains_optional(inner),
        _ => false,
    }
}

/// Whether the leftmost base of an expression chain is an ImportCall (used
/// for the `new import(…)` early error, spec 13.3.4). Parenthesized
/// expressions do not unwrap: `new (import(…))` is a valid NewExpression
/// whose callee is a PrimaryExpression, only the direct `new import(…)` form
/// is excluded.
fn import_call_base(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Member(m) => import_call_base(&m.object),
        ExprKind::Call(c) => import_call_base(&c.callee),
        ExprKind::ImportCall { .. } => true,
        _ => false,
    }
}

/// `( args )` for calls and `new`.
pub(crate) fn parse_arguments(parser: &mut Parser) -> Result<Vec<Argument>, JsError> {
    // Arguments are a fresh expression level: arrows inside them are
    // unconstrained even in a private-in right operand.
    let saved_arrow = parser.reject_arrow;
    parser.reject_arrow = false;
    let result = parse_arguments_inner(parser);
    parser.reject_arrow = saved_arrow;
    result
}

fn parse_arguments_inner(parser: &mut Parser) -> Result<Vec<Argument>, JsError> {
    parser.expect_punct(TokenKind::LeftParen)?;
    let mut args = Vec::new();
    if parser.eat_punct(TokenKind::RightParen)? {
        return Ok(args);
    }
    loop {
        if parser.at_punct(TokenKind::Ellipsis)? {
            let start = parser.next()?.span.start;
            let expr = parse_assignment(parser, true)?;
            let end = expr.span.end;
            args.push(Argument::Spread(Expr {
                span: Span::new(start, end),
                kind: expr.kind,
            }));
        } else {
            args.push(Argument::Expr(parse_assignment(parser, true)?));
        }
        if !parser.eat_punct(TokenKind::Comma)? {
            break;
        }
        if parser.at_punct(TokenKind::RightParen)? {
            break; // trailing comma
        }
    }
    parser.expect_punct(TokenKind::RightParen)?;
    Ok(args)
}

/// Whether a numeric literal's raw text is a legacy octal or non-octal
/// decimal integer (`0777`, `089`) — an early error in strict mode (spec
/// 12.9.3). `0x`, `0b`, `0o`, `0.`, and `0e` forms are unaffected.
fn is_legacy_octal_literal(raw: Vec<u16>) -> bool {
    raw.first() == Some(&0x30) && raw.get(1).is_some_and(|u| (0x30..=0x39).contains(u))
}

/// `PrimaryExpression` (spec 13.2).
fn parse_primary(parser: &mut Parser) -> Result<Expr, JsError> {
    let tok = parser.peek()?.clone();
    match tok.kind {
        TokenKind::Identifier(_) if tok.escaped => {
            // An escaped keyword is an IdentifierName, never the keyword
            // itself (spec 5.1.5); as an expression it must be a valid
            // IdentifierReference, so reserved words are SyntaxErrors here.
            let (name, start) = parser.parse_identifier()?;
            Ok(Expr {
                span: Span::new(start, tok.span.end),
                kind: ExprKind::Ident(name),
            })
        }
        TokenKind::Identifier(atom) => match from_identifier(atom) {
            Some(Keyword::This) => {
                parser.next()?;
                Ok(Expr {
                    span: tok.span,
                    kind: ExprKind::This,
                })
            }
            Some(Keyword::True) => {
                parser.next()?;
                Ok(Expr {
                    span: tok.span,
                    kind: ExprKind::Literal(Literal::Boolean(true)),
                })
            }
            Some(Keyword::False) => {
                parser.next()?;
                Ok(Expr {
                    span: tok.span,
                    kind: ExprKind::Literal(Literal::Boolean(false)),
                })
            }
            Some(Keyword::Null) => {
                parser.next()?;
                Ok(Expr {
                    span: tok.span,
                    kind: ExprKind::Literal(Literal::Null),
                })
            }
            Some(Keyword::Function) => parse_function_expression(parser, false),
            Some(Keyword::Import) => {
                parser.next()?;
                if parser.eat_punct(TokenKind::Dot)? {
                    let meta_tok = parser.peek()?.clone();
                    let (import_prop, _) = parser.parse_identifier()?;
                    if import_prop == intern_utf8("meta") {
                        // `import.met\u0061` — an escape sequence in the
                        // `meta` identifier is a Syntax Error (spec 13.3.7).
                        if meta_tok.escaped {
                            return Err(parser.error_at(
                                meta_tok.span.start,
                                "Escaped 'meta' in import.meta is not allowed",
                            ));
                        }
                        // `import.meta` is an early error in Script code
                        // (spec 13.3.7).
                        if !parser.in_module {
                            return Err(parser.error_at(
                                tok.span.start,
                                "import.meta is not allowed in script code",
                            ));
                        }
                        let end = parser.prev.as_ref().unwrap().span.end;
                        Ok(Expr {
                            span: Span::new(tok.span.start, end),
                            kind: ExprKind::MetaProperty {
                                meta: intern_utf8("import"),
                                property: import_prop,
                            },
                        })
                    } else if import_prop == intern_utf8("source")
                        || import_prop == intern_utf8("defer")
                    {
                        // Source-phase imports (`import.source(x)`) and
                        // deferred imports (`import.defer(x)`) have the same
                        // call shape as `import(x)`; without a host module
                        // loader they reject at runtime.
                        parser.expect_punct(TokenKind::LeftParen)?;
                        let specifier = parse_assignment(parser, true)?;
                        parser.expect_punct(TokenKind::RightParen)?;
                        let end = parser.prev.as_ref().unwrap().span.end;
                        Ok(Expr {
                            span: Span::new(tok.span.start, end),
                            kind: ExprKind::ImportCall {
                                specifier: Box::new(specifier),
                                options: None,
                            },
                        })
                    } else {
                        Err(parser.error_at(tok.span.start, "Expected import.meta"))
                    }
                } else {
                    parser.expect_punct(TokenKind::LeftParen)?;
                    let specifier = parse_assignment(parser, true)?;
                    let options = if parser.eat_punct(TokenKind::Comma)? {
                        if parser.at_punct(TokenKind::RightParen)? {
                            None // trailing comma: `import(x,)`
                        } else {
                            let expr = parse_assignment(parser, true)?;
                            parser.eat_punct(TokenKind::Comma)?; // `import(x, y,)`
                            Some(Box::new(expr))
                        }
                    } else {
                        None
                    };
                    parser.expect_punct(TokenKind::RightParen)?;
                    let end = parser.prev.as_ref().unwrap().span.end;
                    Ok(Expr {
                        span: Span::new(tok.span.start, end),
                        kind: ExprKind::ImportCall {
                            specifier: Box::new(specifier),
                            options,
                        },
                    })
                }
            }
            Some(Keyword::Class) => {
                let start = parser.next()?.span.start;
                let class = crate::class::parse_class(parser, start, false)?;
                Ok(Expr {
                    span: class.span,
                    kind: ExprKind::Class(Box::new(class)),
                })
            }
            Some(_) => {
                let tok = parser.peek()?.clone();
                Err(parser.unexpected(&tok))
            }
            None => {
                let (name, start) = parser.parse_identifier()?;
                Ok(Expr {
                    span: Span::new(start, tok.span.end),
                    kind: ExprKind::Ident(name),
                })
            }
        },
        TokenKind::NullLiteral => {
            parser.next()?;
            Ok(Expr {
                span: tok.span,
                kind: ExprKind::Literal(Literal::Null),
            })
        }
        TokenKind::BooleanLiteral(b) => {
            parser.next()?;
            Ok(Expr {
                span: tok.span,
                kind: ExprKind::Literal(Literal::Boolean(b)),
            })
        }
        TokenKind::NumericLiteral(value) => {
            if parser.strict && is_legacy_octal_literal(parser.source_slice(tok.span)) {
                return Err(parser.error_at(
                    tok.span.start,
                    "Octal literals are not allowed in strict mode",
                ));
            }
            parser.next()?;
            Ok(Expr {
                span: tok.span,
                kind: ExprKind::Literal(match value {
                    syntax::NumericLiteral::Number(n) => Literal::Number(n),
                    syntax::NumericLiteral::BigInt(b) => Literal::BigInt(b),
                }),
            })
        }
        TokenKind::StringLiteral {
            value,
            legacy_octal,
        } => {
            if legacy_octal && parser.strict {
                return Err(parser.error_at(
                    tok.span.start,
                    "Octal escape sequences are not allowed in strict mode",
                ));
            }
            parser.next()?;
            Ok(Expr {
                span: tok.span,
                kind: ExprKind::Literal(Literal::Str(value)),
            })
        }
        TokenKind::RegExpLiteral { pattern, flags } => {
            // Early errors (spec 13.3.2): a regexp literal's pattern and
            // flags must be valid; unknown or duplicate flags and malformed
            // patterns are SyntaxErrors at parse time.
            let parsed_flags = regexp::Flags::parse(flags.as_slice())
                .map_err(|e| parser.error_at(tok.span.start, &e.message))?;
            regexp::compile(pattern.as_slice(), parsed_flags)
                .map_err(|e| parser.error_at(tok.span.start, &e.message))?;
            parser.next()?;
            Ok(Expr {
                span: tok.span,
                kind: ExprKind::Literal(Literal::RegExp { pattern, flags }),
            })
        }
        TokenKind::NoSubstitutionTemplate { cooked, raw } => {
            parser.next()?;
            // spec 13.2.8.1: an untagged template whose TV is undefined (it
            // contains an invalid escape sequence) is a SyntaxError.
            if cooked.is_none() {
                return Err(parser.error_at(
                    tok.span.start,
                    "Invalid escape sequence in untagged template literal",
                ));
            }
            let element = TemplateElement {
                cooked,
                raw,
                span: tok.span,
            };
            Ok(Expr {
                span: tok.span,
                kind: ExprKind::Template(TemplateLiteral {
                    quasis: vec![element],
                    exprs: Vec::new(),
                    span: tok.span,
                }),
            })
        }
        TokenKind::TemplateHead { cooked, raw } => {
            let saved_arrow = parser.reject_arrow;
            parser.reject_arrow = false;
            parser.next()?;
            let result = parse_template_rest(parser, cooked, raw, tok.span);
            parser.reject_arrow = saved_arrow;
            result
        }
        TokenKind::LeftParen => {
            let saved_arrow = parser.reject_arrow;
            // The contents of a parenthesized expression are a fresh
            // expression level, so arrows inside are unconstrained.
            parser.reject_arrow = false;
            parser.next()?;
            let result = parse_paren_contents(parser)?;
            parser.reject_arrow = saved_arrow;
            match result {
                ParenResult::Empty => Err(parser.error_at(tok.span.start, "Unexpected token ')'")),
                ParenResult::Expr(inner) => Ok(Expr {
                    span: tok.span,
                    kind: ExprKind::Paren(Box::new(inner)),
                }),
                ParenResult::ArrowParams(params) => {
                    // An arrow-function parameter list is only a valid
                    // expression when the enclosing production allows an
                    // AssignmentExpression; the private-in right operand is
                    // a ShiftExpression (spec 13.11).
                    if saved_arrow {
                        return Err(parser.error_at(
                            tok.span.start,
                            "Arrow function is not allowed in a private-in expression",
                        ));
                    }
                    parser.expect_punct(TokenKind::Arrow)?;
                    let body = parse_arrow_body(parser, false, &params)?;
                    let end = parser.prev.as_ref().unwrap().span.end;
                    Ok(Expr {
                        span: Span::new(tok.span.start, end),
                        kind: ExprKind::Arrow {
                            is_async: false,
                            params,
                            body,
                        },
                    })
                }
            }
        }
        TokenKind::LeftBracket => {
            let saved_arrow = parser.reject_arrow;
            parser.reject_arrow = false;
            let result = parse_array_literal(parser);
            parser.reject_arrow = saved_arrow;
            result
        }
        TokenKind::LeftBrace => {
            let saved_arrow = parser.reject_arrow;
            parser.reject_arrow = false;
            let result = parse_object_literal(parser);
            parser.reject_arrow = saved_arrow;
            result
        }
        _ => {
            let tok = parser.peek()?.clone();
            Err(parser.unexpected(&tok))
        }
    }
}

/// Parses the substitution/tail sequence of a template whose head has been
/// consumed.
fn parse_template_rest(
    parser: &mut Parser,
    head_cooked: Option<crux::JsString>,
    head_raw: crux::JsString,
    head_span: Span,
) -> Result<Expr, JsError> {
    let start = head_span.start;
    if head_cooked.is_none() {
        return Err(parser.error_at(
            start,
            "Invalid escape sequence in untagged template literal",
        ));
    }
    let mut quasis = vec![TemplateElement {
        cooked: head_cooked,
        raw: head_raw,
        span: head_span,
    }];
    let mut exprs = Vec::new();
    loop {
        let expr = parse_expression(parser, true)?;
        exprs.push(expr);
        let tail = parser.next_with_goal(syntax::LexGoal::TemplateTail)?;
        match tail.kind {
            TokenKind::TemplateMiddle { cooked, raw } => {
                if cooked.is_none() {
                    return Err(parser.error_at(
                        tail.span.start,
                        "Invalid escape sequence in untagged template literal",
                    ));
                }
                quasis.push(TemplateElement {
                    cooked,
                    raw,
                    span: tail.span,
                });
            }
            TokenKind::TemplateTail { cooked, raw } => {
                if cooked.is_none() {
                    return Err(parser.error_at(
                        tail.span.start,
                        "Invalid escape sequence in untagged template literal",
                    ));
                }
                quasis.push(TemplateElement {
                    cooked,
                    raw,
                    span: tail.span,
                });
                break;
            }
            _ => return Err(parser.unexpected(&tail)),
        }
    }
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Expr {
        span: Span::new(start, end),
        kind: ExprKind::Template(TemplateLiteral {
            quasis,
            exprs,
            span: Span::new(start, end),
        }),
    })
}

/// A tagged template: `` expr`…` ``.
fn parse_tagged_template(parser: &mut Parser, tag: Expr) -> Result<Expr, JsError> {
    if contains_optional(&tag) {
        return Err(parser.error_at(
            tag.span.start,
            "Tagged templates are not allowed on an optional chain",
        ));
    }
    let start = tag.span.start;
    let tok = parser.peek()?.clone();
    match tok.kind {
        TokenKind::NoSubstitutionTemplate { cooked, raw } => {
            parser.next()?;
            let quasi = TemplateLiteral {
                quasis: vec![TemplateElement {
                    cooked,
                    raw,
                    span: tok.span,
                }],
                exprs: Vec::new(),
                span: tok.span,
            };
            Ok(Expr {
                span: Span::new(start, tok.span.end),
                kind: ExprKind::TaggedTemplate {
                    tag: Box::new(tag),
                    quasi,
                },
            })
        }
        TokenKind::TemplateHead { cooked, raw } => {
            parser.next()?;
            let mut quasis = vec![TemplateElement {
                cooked,
                raw,
                span: tok.span,
            }];
            let mut exprs = Vec::new();
            loop {
                let expr = parse_expression(parser, true)?;
                exprs.push(expr);
                let tail = parser.next_with_goal(syntax::LexGoal::TemplateTail)?;
                match tail.kind {
                    TokenKind::TemplateMiddle { cooked, raw } => {
                        quasis.push(TemplateElement {
                            cooked,
                            raw,
                            span: tail.span,
                        });
                    }
                    TokenKind::TemplateTail { cooked, raw } => {
                        quasis.push(TemplateElement {
                            cooked,
                            raw,
                            span: tail.span,
                        });
                        break;
                    }
                    _ => return Err(parser.unexpected(&tail)),
                }
            }
            let end = parser.prev.as_ref().unwrap().span.end;
            Ok(Expr {
                span: Span::new(start, end),
                kind: ExprKind::TaggedTemplate {
                    tag: Box::new(tag),
                    quasi: TemplateLiteral {
                        quasis,
                        exprs,
                        span: Span::new(start, end),
                    },
                },
            })
        }
        _ => Err(parser.unexpected(&tok)),
    }
}

/// Parses `( … )` at an expression start: either a parenthesized expression
/// or arrow-function parameters (the cover grammar, spec 13.2.2).
pub(crate) fn parse_paren_contents(parser: &mut Parser) -> Result<ParenResult, JsError> {
    let saved_cover = parser.in_arrow_cover;
    let saved_error = parser.cover_error;
    parser.in_arrow_cover = true;
    parser.cover_error = None;
    parser.suppress_cover_raise += 1;

    let mut items: Vec<ParenItem> = Vec::new();
    let mut trailing_comma = false;
    if !parser.eat_punct(TokenKind::RightParen)? {
        loop {
            if parser.at_punct(TokenKind::Ellipsis)? {
                parser.next()?;
                let expr = parse_assignment(parser, true)?;
                items.push(ParenItem::Spread(expr));
            } else {
                items.push(ParenItem::Expr(parse_assignment(parser, true)?));
            }
            if parser.eat_punct(TokenKind::Comma)? {
                if parser.at_punct(TokenKind::RightParen)? {
                    trailing_comma = true;
                    break;
                }
                continue;
            }
            break;
        }
        parser.expect_punct(TokenKind::RightParen)?;
    }
    parser.suppress_cover_raise -= 1;

    let is_arrow = parser.at_punct(TokenKind::Arrow)? && !parser.peek()?.line_break_before;
    if is_arrow {
        // `(…, ...rest,)` — a trailing comma after a rest parameter is never
        // part of the ArrowFormalParameters grammar (spec 15.4.1).
        if trailing_comma
            && items
                .last()
                .is_some_and(|item| matches!(item, ParenItem::Spread(_)))
        {
            return Err(parser.error_at(
                parser.prev.as_ref().unwrap().span.start,
                "Trailing comma is not allowed after a rest parameter",
            ));
        }
        let params = items_to_params(parser, items)?;
        check_arrow_params(parser, &params)?;
        parser.cover_error = saved_error;
        parser.in_arrow_cover = saved_cover;
        return Ok(ParenResult::ArrowParams(params));
    }

    // Plain parenthesized expression: cover-only forms are errors here.
    parser.in_arrow_cover = saved_cover;
    if let Some(span) = parser.cover_error {
        parser.cover_error = saved_error;
        return Err(parser.error_at(span.start, "Invalid shorthand property initializer"));
    }
    parser.cover_error = saved_error;
    if trailing_comma {
        return Err(parser.error_at(
            parser.prev.as_ref().unwrap().span.start,
            "Unexpected trailing comma",
        ));
    }
    if items.is_empty() {
        return Ok(ParenResult::Empty);
    }
    if items.len() == 1 {
        return match items.pop().unwrap() {
            ParenItem::Expr(expr) => Ok(ParenResult::Expr(expr)),
            ParenItem::Spread(_) => Err(parser.error_at(
                parser.prev.as_ref().unwrap().span.start,
                "Unexpected token '...'",
            )),
        };
    }
    // `(a, b)` — a sequence expression.
    let exprs = items
        .into_iter()
        .map(|item| match item {
            ParenItem::Expr(expr) => Ok(expr),
            ParenItem::Spread(_) => Err(parser.error_at(
                parser.prev.as_ref().unwrap().span.start,
                "Unexpected token '...'",
            )),
        })
        .collect::<Result<Vec<_>, JsError>>()?;
    let start = exprs.first().unwrap().span.start;
    let end = exprs.last().unwrap().span.end;
    Ok(ParenResult::Expr(Expr {
        span: Span::new(start, end),
        kind: ExprKind::Sequence(exprs),
    }))
}

/// Converts cover items to an arrow parameter list.
fn items_to_params(
    parser: &mut Parser,
    items: Vec<ParenItem>,
) -> Result<Vec<BindingElement>, JsError> {
    let mut params = Vec::new();
    let n = items.len();
    for (i, item) in items.into_iter().enumerate() {
        match item {
            ParenItem::Expr(expr) => params.push(expr_to_binding_element(parser, expr)?),
            ParenItem::Spread(expr) => {
                if i + 1 != n {
                    return Err(parser.error_at(expr.span.start, "Rest parameter must be last"));
                }
                let span = expr.span;
                let pattern = expr_to_pattern(parser, expr)?;
                params.push(BindingElement {
                    pattern,
                    init: None,
                    rest: true,
                    span,
                });
            }
        }
    }
    // Arrow parameters are always unique.
    check_duplicate_params(parser, &params, true)?;
    Ok(params)
}

/// spec 15.4.1 ArrowFunction early errors: ArrowParameters must not contain
/// a YieldExpression or an AwaitExpression, and in strict mode may not bind
/// `eval` or `arguments`.
fn check_arrow_params(parser: &mut Parser, params: &[BindingElement]) -> Result<(), JsError> {
    for p in params {
        if let Some(init) = &p.init {
            let mut found = false;
            crate::early_errors::walk_exprs(init, &mut |e| {
                if matches!(e.kind, ExprKind::Yield { .. } | ExprKind::Await(_)) {
                    found = true;
                }
            });
            if found {
                return Err(parser.error_at(
                    init.span.start,
                    "Await or yield is not allowed in arrow-function parameters",
                ));
            }
        }
        if parser.strict {
            for name in crate::stmt::bound_names(&p.pattern) {
                if name == intern_utf8("eval") || name == intern_utf8("arguments") {
                    return Err(parser
                        .error_at(p.span.start, "Unexpected eval or arguments in strict mode"));
                }
            }
        }
    }
    Ok(())
}

/// Converts a cover expression to a binding element (arrow param).
fn expr_to_binding_element(parser: &mut Parser, expr: Expr) -> Result<BindingElement, JsError> {
    let span = expr.span;
    match expr.kind {
        ExprKind::Ident(name) => Ok(BindingElement {
            pattern: BindingPattern::Ident(name),
            init: None,
            rest: false,
            span,
        }),
        ExprKind::Assign {
            op: AssignOp::Assign,
            target,
            value,
        } => {
            let pattern = expr_to_pattern(parser, *target)?;
            Ok(BindingElement {
                pattern,
                init: Some(*value),
                rest: false,
                span,
            })
        }
        other => {
            let pattern = expr_to_pattern(parser, Expr { span, kind: other })?;
            Ok(BindingElement {
                pattern,
                init: None,
                rest: false,
                span,
            })
        }
    }
}

/// Converts an expression into a binding pattern (for destructuring targets).
fn expr_to_pattern(parser: &mut Parser, expr: Expr) -> Result<BindingPattern, JsError> {
    match expr.kind {
        ExprKind::Ident(name) => Ok(BindingPattern::Ident(name)),
        ExprKind::Array(lit) => {
            let mut elements = Vec::new();
            let mut seen_rest = false;
            for el in lit.elements {
                match el {
                    ArrayElement::Hole => {
                        if seen_rest {
                            return Err(
                                parser.error_at(expr.span.start, "Rest element must be last")
                            );
                        }
                        elements.push(ArrayBindingElement::Hole);
                    }
                    ArrayElement::Expr(e) => {
                        if seen_rest {
                            return Err(
                                parser.error_at(expr.span.start, "Rest element must be last")
                            );
                        }
                        elements.push(ArrayBindingElement::Element(expr_to_binding_element(
                            parser, e,
                        )?));
                    }
                    ArrayElement::Spread(e) => {
                        if seen_rest {
                            return Err(parser.error_at(e.span.start, "Rest element must be last"));
                        }
                        let span = e.span;
                        let pattern = expr_to_pattern(parser, e)?;
                        elements.push(ArrayBindingElement::Rest(BindingElement {
                            pattern,
                            init: None,
                            rest: false,
                            span,
                        }));
                        seen_rest = true;
                    }
                }
            }
            Ok(BindingPattern::Array(elements))
        }
        ExprKind::Object(lit) => {
            let mut props = Vec::new();
            let mut seen_rest = false;
            for prop in lit.props {
                match prop {
                    ObjectProperty::Init {
                        key,
                        value,
                        shorthand: _,
                    } => {
                        if seen_rest {
                            return Err(
                                parser.error_at(value.span.start, "Rest element must be last")
                            );
                        }
                        let span = value.span;
                        let element = expr_to_binding_element(parser, value)?;
                        props.push(ObjectBindingProperty::Property { key, element, span });
                    }
                    ObjectProperty::Spread(e) => {
                        let span = e.span;
                        let pattern = expr_to_pattern(parser, e)?;
                        props.push(ObjectBindingProperty::Rest(BindingElement {
                            pattern,
                            init: None,
                            rest: false,
                            span,
                        }));
                        seen_rest = true;
                    }
                    _ => {
                        return Err(
                            parser.error_at(expr.span.start, "Invalid destructuring target")
                        );
                    }
                }
            }
            Ok(BindingPattern::Object(props))
        }
        _ => Err(parser.error_at(expr.span.start, "Invalid destructuring target")),
    }
}

/// `[ … ]` array literal.
/// `[ … ]` array literal. The literal may still be disambiguated into an
/// assignment pattern, so element-level cover errors are deferred to the
/// enclosing construct.
fn parse_array_literal(parser: &mut Parser) -> Result<Expr, JsError> {
    parser.suppress_cover_raise += 1;
    let result = parse_array_literal_inner(parser);
    parser.suppress_cover_raise -= 1;
    result
}

fn parse_array_literal_inner(parser: &mut Parser) -> Result<Expr, JsError> {
    let start = parser.next()?.span.start; // '['
    let mut elements: Vec<ArrayElement> = Vec::new();
    let mut rest_trailing_comma = false;
    while !parser.at_punct(TokenKind::RightBracket)? {
        if parser.at_punct(TokenKind::Comma)? {
            parser.next()?;
            elements.push(ArrayElement::Hole);
            continue;
        }
        if parser.eat_punct(TokenKind::Ellipsis)? {
            let expr = parse_assignment(parser, true)?;
            elements.push(ArrayElement::Spread(expr));
            // `[...x, ]`: a comma directly after the rest marks a trailing
            // elision — an early error when the array is a pattern, and a
            // harmless no-op in expression position.
            if parser.at_punct(TokenKind::Comma)? && parser.peek2()?.kind == TokenKind::RightBracket
            {
                rest_trailing_comma = true;
            }
        } else {
            elements.push(ArrayElement::Expr(parse_assignment(parser, true)?));
        }
        if !parser.eat_punct(TokenKind::Comma)? {
            break;
        }
    }
    parser.expect_punct(TokenKind::RightBracket)?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Expr {
        span: Span::new(start, end),
        kind: ExprKind::Array(ArrayLiteral {
            elements,
            rest_trailing_comma,
            span: Span::new(start, end),
        }),
    })
}

/// Whether `atom` cannot be an IdentifierReference in the current context
/// (spec 13.1.1): keywords always, future-reserved words in strict mode, and
/// `yield`/`await` in resumable or module code.
fn is_reference_identifier_error(parser: &Parser, atom: AtomId) -> bool {
    from_identifier(atom).is_some()
        || (parser.strict && is_future_reserved_word(atom))
        || (parser.strict && (atom == intern_utf8("eval") || atom == intern_utf8("arguments")))
        || (parser.in_generator && atom == intern_utf8("yield"))
        || ((parser.in_async || parser.in_module) && atom == intern_utf8("await"))
}

/// `{ … }` object literal, with cover-initialized names (spec 13.2.5). The
/// literal may still be disambiguated into an assignment pattern, so a
/// CoverInitializedName defers its error to the enclosing construct.
fn parse_object_literal(parser: &mut Parser) -> Result<Expr, JsError> {
    parser.suppress_cover_raise += 1;
    let result = parse_object_literal_inner(parser);
    parser.suppress_cover_raise -= 1;
    result
}

fn parse_object_literal_inner(parser: &mut Parser) -> Result<Expr, JsError> {
    let start = parser.next()?.span.start; // '{'
    let mut props: Vec<ObjectProperty> = Vec::new();
    let mut rest_trailing_comma = false;
    while !parser.at_punct(TokenKind::RightBrace)? {
        if parser.eat_punct(TokenKind::Ellipsis)? {
            let expr = parse_assignment(parser, true)?;
            props.push(ObjectProperty::Spread(expr));
            // `{...x, }`: a comma directly after the rest marks a trailing
            // element — an early error when the object is a pattern, and a
            // harmless no-op in expression position.
            if parser.at_punct(TokenKind::Comma)? && parser.peek2()?.kind == TokenKind::RightBrace {
                rest_trailing_comma = true;
            }
            if !parser.eat_punct(TokenKind::Comma)? {
                break;
            }
            continue;
        }
        let prop_start = parser.peek()?.span.start;

        // `*name() {}` generator method.
        if parser.at_punct(TokenKind::Star)? {
            parser.next()?;
            let key = parser.parse_property_name()?;
            let function = parse_method_tail(parser, prop_start, false, true)?;
            props.push(ObjectProperty::Method { key, function });
            if !parser.eat_punct(TokenKind::Comma)? {
                break;
            }
            continue;
        }
        // `async name() {}` — but `async` alone or `async:` is a normal prop.
        if parser.at_contextual("async")?
            && !parser.peek2()?.line_break_before
            && (is_property_name_start(parser.peek2()?.kind.clone())
                || matches!(parser.peek2()?.kind, TokenKind::Star))
        {
            parser.next()?; // `async`
            let is_generator = parser.eat_punct(TokenKind::Star)?;
            let key = parser.parse_property_name()?;
            let function = parse_method_tail(parser, prop_start, true, is_generator)?;
            props.push(ObjectProperty::Method { key, function });
            if !parser.eat_punct(TokenKind::Comma)? {
                break;
            }
            continue;
        }
        // `get` name() {}` / `set name(p) {}` accessors.
        if parser.at_contextual("get")? && is_property_name_start(parser.peek2()?.kind.clone()) {
            parser.next()?; // `get`
            let key = parser.parse_property_name()?;
            parser.expect_punct(TokenKind::LeftParen)?;
            parser.expect_punct(TokenKind::RightParen)?;
            let body = parse_function_body_block(parser, false, false, &[], true, false, false)?;
            props.push(ObjectProperty::Get { key, body: body.0 });
            if !parser.eat_punct(TokenKind::Comma)? {
                break;
            }
            continue;
        }
        if parser.at_contextual("set")? && is_property_name_start(parser.peek2()?.kind.clone()) {
            parser.next()?; // `set`
            let key = parser.parse_property_name()?;
            parser.expect_punct(TokenKind::LeftParen)?;
            // A setter takes a single FormalParameter, which may carry a
            // default initializer (`set x(v = 1) {}`, spec 13.3.5). Its
            // params and body parse with [~Yield, ~Await]: a static block's
            // [+Await] does not leak in (`set accessor(await) {}` is valid).
            let saved = (parser.in_generator, parser.in_async, parser.allow_super);
            parser.in_generator = false;
            parser.in_async = false;
            parser.allow_super = true;
            let param_element = parser.parse_binding_element()?;
            (parser.in_generator, parser.in_async, parser.allow_super) = saved;
            parser.expect_punct(TokenKind::RightParen)?;
            let body = parse_function_body_block(
                parser,
                false,
                false,
                std::slice::from_ref(&param_element),
                true,
                false,
                false,
            )?;
            props.push(ObjectProperty::Set {
                key,
                param: param_element.pattern,
                init: param_element.init,
                body: body.0,
            });
            if !parser.eat_punct(TokenKind::Comma)? {
                break;
            }
            continue;
        }

        let key = parser.parse_property_name()?;
        if parser.at_punct(TokenKind::LeftParen)? {
            // Plain method: `name() {}`.
            let function = parse_method_tail(parser, prop_start, false, false)?;
            props.push(ObjectProperty::Method { key, function });
        } else if parser.eat_punct(TokenKind::Colon)? {
            let value = parse_assignment(parser, true)?;
            props.push(ObjectProperty::Init {
                key,
                value,
                shorthand: false,
            });
        } else if parser.eat_punct(TokenKind::Equal)? {
            // CoverInitializedName: `key = value` — only legal when the
            // object is disambiguated into a pattern.
            let PropertyName::Ident(name) = key else {
                return Err(parser.error_at(prop_start, "Invalid shorthand property initializer"));
            };
            if is_reference_identifier_error(parser, name) {
                return Err(parser.error_at(prop_start, "Unexpected reserved word"));
            }
            let value = parse_assignment(parser, true)?;
            let span = Span::new(prop_start, parser.prev.as_ref().unwrap().span.end);
            let shorthand = Expr {
                span,
                kind: ExprKind::Assign {
                    op: AssignOp::Assign,
                    target: Box::new(Expr {
                        span,
                        kind: ExprKind::Ident(name),
                    }),
                    value: Box::new(value),
                },
            };
            if parser.cover_error.is_none() {
                parser.cover_error = Some(span);
            }
            props.push(ObjectProperty::Init {
                key: PropertyName::Ident(name),
                value: shorthand,
                shorthand: true,
            });
        } else {
            // Shorthand: `{ x }` — the identifier is a reference, so reserved
            // words are rejected (spec 13.1.1).
            let PropertyName::Ident(name) = key else {
                return Err(parser.error_at(prop_start, "Invalid shorthand property name"));
            };
            if is_reference_identifier_error(parser, name) {
                return Err(parser.error_at(prop_start, "Unexpected reserved word"));
            }
            let end = parser.prev.as_ref().unwrap().span.end;
            props.push(ObjectProperty::Init {
                key: PropertyName::Ident(name),
                value: Expr {
                    span: Span::new(prop_start, end),
                    kind: ExprKind::Ident(name),
                },
                shorthand: true,
            });
        }
        if !parser.eat_punct(TokenKind::Comma)? {
            break;
        }
    }
    parser.expect_punct(TokenKind::RightBrace)?;
    let end = parser.prev.as_ref().unwrap().span.end;
    Ok(Expr {
        span: Span::new(start, end),
        kind: ExprKind::Object(ObjectLiteral {
            props,
            rest_trailing_comma,
            span: Span::new(start, end),
        }),
    })
}

/// Whether a token can begin a property name in a method/accessor header.
pub(crate) fn is_property_name_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier(_)
            | TokenKind::StringLiteral { .. }
            | TokenKind::NumericLiteral(_)
            | TokenKind::LeftBracket
    )
}

/// After the method name, parses `( params ) { body }`.
fn parse_method_tail(
    parser: &mut Parser,
    start: u32,
    is_async: bool,
    is_generator: bool,
) -> Result<Function, JsError> {
    parser.expect_punct(TokenKind::LeftParen)?;
    // Methods have a [[HomeObject]], so `super` is available in the formal
    // parameters as well as the body (spec 13.3.5).
    let saved = (
        parser.in_generator,
        parser.in_async,
        parser.allow_super,
        parser.top_level_await,
    );
    parser.in_generator = is_generator;
    parser.in_async = is_async;
    parser.allow_super = true;
    parser.top_level_await = false;
    let params = parse_parameter_list(parser)?;
    (
        parser.in_generator,
        parser.in_async,
        parser.allow_super,
        parser.top_level_await,
    ) = saved;
    // Method definitions are always strict mode code, so duplicate parameter
    // names are an early error even in sloppy code (spec 13.3.5).
    check_duplicate_params(parser, &params, true)?;
    check_function_params(parser, &params, is_async, is_generator)?;
    let (body, _) =
        parse_function_body_block(parser, is_async, is_generator, &params, true, false, false)?;
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

/// `function` expression: `function [name] ( params ) { body }` incl.
/// `async function` and generators.
pub(crate) fn parse_function_expression(
    parser: &mut Parser,
    is_async: bool,
) -> Result<Expr, JsError> {
    // The span covers the whole definition; an async function's caller has
    // already consumed the `async` keyword, so it is `parser.prev` there.
    let start = if is_async {
        parser.prev.as_ref().unwrap().span.start
    } else {
        parser.peek()?.span.start
    };
    parser.next()?; // `function`
    let is_generator = parser.eat_punct(TokenKind::Star)?;
    let name = parse_function_expression_name(parser, is_generator, is_async)?;
    parser.expect_punct(TokenKind::LeftParen)?;
    // Params parse with the function's own [Yield, Await] grammar (spec
    // 15.4.1): generator params reserve `yield`, async params reserve
    // `await`, and a plain function's params reset both regardless of the
    // enclosing context (a static block's [+Await] does not leak in).
    let saved = (parser.in_generator, parser.in_async, parser.top_level_await);
    parser.in_generator = is_generator;
    parser.in_async = is_async;
    parser.top_level_await = false;
    let params = parse_parameter_list(parser)?;
    (parser.in_generator, parser.in_async, parser.top_level_await) = saved;
    check_duplicate_params(parser, &params, false)?;
    check_function_params(parser, &params, is_async, is_generator)?;
    let (body, strict) =
        parse_function_body_block(parser, is_async, is_generator, &params, false, false, false)?;
    if strict {
        check_function_name_strict(parser, name, start)?;
    }
    let end = body.span.end;
    Ok(Expr {
        span: Span::new(start, end),
        kind: ExprKind::Function(Function {
            span: Span::new(start, end),
            name,
            params,
            body,
            is_async,
            is_generator,
            statement_position: false,
        }),
    })
}

/// The optional name of a function expression, with the per-form `yield`/
/// `await` rules (spec 15.4.1): a FunctionExpression's name is
/// `BindingIdentifier[~Yield, ~Await]` (so `yield` and `await` are ordinary
/// names even in resumable code), a GeneratorExpression's name rejects
/// `yield` in all contexts, and an AsyncGeneratorExpression's name rejects
/// both. The strict-mode name checks (`eval`/`arguments` and the strict
/// reserved words) are deferred until the body's strictness is known.
fn parse_function_expression_name(
    parser: &mut Parser,
    is_generator: bool,
    is_async: bool,
) -> Result<Option<AtomId>, JsError> {
    if !matches!(parser.peek()?.kind, TokenKind::Identifier(_)) {
        return Ok(None);
    }
    let saved = (parser.in_generator, parser.in_async, parser.strict);
    match (is_generator, is_async) {
        (false, false) => {
            parser.in_generator = false;
            parser.in_async = false;
        }
        (true, false) => {
            parser.in_generator = true;
            parser.in_async = false;
        }
        (false, true) => {}
        (true, true) => {
            parser.in_generator = true;
            parser.in_async = true;
        }
    }
    parser.strict = false;
    let name = if parser.at_identifier()? {
        Some(parser.parse_identifier()?.0)
    } else {
        None
    };
    (parser.in_generator, parser.in_async, parser.strict) = saved;
    Ok(name)
}

/// The strict-mode BindingIdentifier restrictions for a function-expression
/// or function-declaration name (spec 15.4.1): `eval`/`arguments` and the
/// strict reserved words are forbidden when the name is contained in strict
/// mode code.
pub(crate) fn check_function_name_strict(
    parser: &Parser,
    name: Option<AtomId>,
    start: u32,
) -> Result<(), JsError> {
    let Some(name) = name else {
        return Ok(());
    };
    if name == intern_utf8("eval") || name == intern_utf8("arguments") {
        return Err(parser.error_at(start, "Unexpected eval or arguments in strict mode"));
    }
    if syntax::keywords::is_future_reserved_word(name) {
        return Err(parser.error_at(start, "Unexpected reserved word"));
    }
    Ok(())
}

/// Parses `( params )` — the caller has consumed `(`.
pub(crate) fn parse_parameter_list(parser: &mut Parser) -> Result<Vec<BindingElement>, JsError> {
    let mut params = Vec::new();
    if parser.eat_punct(TokenKind::RightParen)? {
        return Ok(params);
    }
    loop {
        if parser.at_punct(TokenKind::Ellipsis)? {
            parser.next()?;
            let start = parser.prev.as_ref().unwrap().span.start;
            let pattern = parser.parse_binding_pattern()?;
            if parser.at_punct(TokenKind::Equal)? {
                return Err(
                    parser.error_at(start, "Rest parameter may not have a default initializer")
                );
            }
            let end = parser.prev.as_ref().unwrap().span.end;
            params.push(BindingElement {
                pattern,
                init: None,
                rest: true,
                span: Span::new(start, end),
            });
            parser.expect_punct(TokenKind::RightParen)?;
            return Ok(params);
        }
        params.push(parser.parse_binding_element()?);
        if !parser.eat_punct(TokenKind::Comma)? {
            break;
        }
        if parser.at_punct(TokenKind::RightParen)? {
            break; // trailing comma
        }
    }
    parser.expect_punct(TokenKind::RightParen)?;
    Ok(params)
}

/// Parses `{ FunctionBody }` with the directive prologue and the function
/// context flags. `params` are declared in the function scope and checked
/// against the body's lexical declarations. `allow_super`/`in_constructor`
/// govern the `super` forms legal in the body. Returns the body and whether
/// the function's code is strict (enclosing strictness or a `"use strict"`
/// directive in the prologue).
pub(crate) fn parse_function_body_block(
    parser: &mut Parser,
    is_async: bool,
    is_generator: bool,
    params: &[BindingElement],
    allow_super: bool,
    in_constructor: bool,
    is_arrow: bool,
) -> Result<(Block, bool), JsError> {
    parser.expect_punct(TokenKind::LeftBrace)?;
    let body_start = parser.prev.as_ref().unwrap().span.start;
    let directive_strict = scan_directive_prologue(parser)?;
    if directive_strict && !is_simple_params(params) {
        return Err(parser.error_at(
            body_start,
            "Illegal 'use strict' directive in function with non-simple parameter list",
        ));
    }
    let strict = parser.strict || directive_strict;

    let saved = (
        parser.strict,
        parser.in_function,
        parser.in_generator,
        parser.in_async,
        parser.allow_super,
        parser.in_constructor,
        parser.top_level_await,
        parser.nt_context,
    );
    parser.strict = strict;
    parser.in_function = true;
    parser.in_generator = is_generator;
    parser.in_async = is_async;
    parser.allow_super = allow_super;
    parser.in_constructor = in_constructor;
    parser.top_level_await = false;
    // Arrows do not establish a new.target context of their own: they
    // inherit from the enclosing function (spec 13.3.4).
    parser.nt_context = !is_arrow || parser.nt_context;
    if directive_strict {
        // A `"use strict"` directive makes the already-parsed parameter
        // list strict: `eval`/`arguments` bindings and duplicates become
        // early errors (spec 15.2.1).
        for name in crate::stmt::bound_names_of_elements(params) {
            if name == intern_utf8("eval") || name == intern_utf8("arguments") {
                return Err(
                    parser.error_at(body_start, "Unexpected eval or arguments in strict mode")
                );
            }
        }
        check_duplicate_params(parser, params, false)?;
    }
    let saved_vars = std::mem::take(&mut parser.list_vars);
    parser.push_scope();
    parser.scopes.last_mut().unwrap().is_function = true;
    for name in crate::stmt::bound_names_of_elements(params) {
        parser.scopes.last_mut().unwrap().params.insert(name);
    }
    let stmts = crate::stmt::parse_statement_list(parser, TokenKind::RightBrace)?;
    parser.expect_punct(TokenKind::RightBrace)?;
    let end = parser.prev.as_ref().unwrap().span.end;
    parser.pop_scope();
    // `var` names in a function body do not propagate past it.
    parser.list_vars = saved_vars;
    (
        parser.strict,
        parser.in_function,
        parser.in_generator,
        parser.in_async,
        parser.allow_super,
        parser.in_constructor,
        parser.top_level_await,
        parser.nt_context,
    ) = saved;
    Ok((
        Block {
            stmts,
            span: Span::new(body_start, end),
        },
        strict,
    ))
}

/// Whether a parameter list is simple (all plain binding identifiers).
fn is_simple_params(params: &[BindingElement]) -> bool {
    params
        .iter()
        .all(|p| !p.rest && p.init.is_none() && matches!(p.pattern, BindingPattern::Ident(_)))
}

/// spec 15.2.2.1: a generator function's FormalParameters must not contain a
/// YieldExpression, and an async or async-generator function's FormalParameters
/// must not contain an AwaitExpression. The params parse with the function's
/// own [Yield]/[Await] grammar (the callers set `parser.in_generator` and
/// `parser.in_async` first), so `yield`/`await` in a default initializer is a
/// YieldExpression/AwaitExpression node to walk (instance-yield-expr-in-param,
/// instance-await-expr-in-param).
pub(crate) fn check_function_params(
    parser: &mut Parser,
    params: &[BindingElement],
    is_async: bool,
    is_generator: bool,
) -> Result<(), JsError> {
    for p in params {
        let Some(init) = &p.init else {
            continue;
        };
        let mut found_yield = false;
        let mut found_await = false;
        crate::early_errors::walk_exprs(init, &mut |e| {
            if matches!(e.kind, ExprKind::Yield { .. }) {
                found_yield = true;
            }
            if matches!(e.kind, ExprKind::Await(_)) {
                found_await = true;
            }
        });
        if is_generator && found_yield {
            return Err(parser.error_at(
                init.span.start,
                "Yield expression not allowed in generator parameters",
            ));
        }
        if is_async && found_await {
            return Err(parser.error_at(
                init.span.start,
                "Await expression not allowed in async function parameters",
            ));
        }
    }
    Ok(())
}

/// Validates a parameter list: duplicates are an error in strict mode, for
/// non-simple lists, and always for arrows. Returns whether the list is
/// simple.
pub(crate) fn check_duplicate_params(
    parser: &mut Parser,
    params: &[BindingElement],
    always_unique: bool,
) -> Result<bool, JsError> {
    let simple = is_simple_params(params);
    let mut names = std::collections::HashSet::new();
    let mut dup = false;
    for p in params {
        for name in crate::stmt::bound_names(&p.pattern) {
            if !names.insert(name) {
                dup = true;
            }
        }
    }
    if dup && (always_unique || parser.strict || !simple) {
        return Err(parser.error_at(
            params.first().map_or(0, |p| p.span.start),
            "Duplicate parameter name not allowed in this context",
        ));
    }
    Ok(simple)
}

/// Scans the directive prologue for a `"use strict"` directive, without
/// consuming tokens.
pub(crate) fn scan_directive_prologue(parser: &mut Parser) -> Result<bool, JsError> {
    let snapshot = parser.snapshot();
    let mut saw_strict = false;
    loop {
        let tok = parser.next()?;
        let TokenKind::StringLiteral { value, .. } = tok.kind else {
            break;
        };
        // A Use Strict Directive must not contain escapes (spec 14.1.1), but
        // an escaped string is still a directive: the prologue continues past
        // it (`"\x41"; "use strict"` is strict).
        let raw = parser.source_slice(tok.span);
        if !raw.contains(&0x5C_u16) && value.to_string_lossy() == "use strict" {
            saw_strict = true;
        }
        let next = parser.next()?;
        match next.kind {
            TokenKind::Semicolon => continue,
            _ if next.line_break_before
                || matches!(next.kind, TokenKind::RightBrace | TokenKind::Eof) =>
            {
                continue;
            }
            _ => break,
        }
    }
    parser.restore(snapshot);
    Ok(saw_strict)
}

/// The arrow body: `=> ConciseBody` — a block or an assignment expression.
/// Arrows capture the enclosing `super` binding.
fn parse_arrow_body(
    parser: &mut Parser,
    is_async: bool,
    params: &[BindingElement],
) -> Result<ArrowBody, JsError> {
    // `=>` consumed by the caller.
    if parser.at_punct(TokenKind::LeftBrace)? {
        let (body, _) = parse_function_body_block(
            parser,
            is_async,
            false,
            params,
            parser.allow_super,
            parser.in_constructor,
            true,
        )?;
        Ok(ArrowBody::Block(body))
    } else {
        // A concise body is a function body: `await`/`yield`/top-level
        // await are governed by the arrow's own flags, not the enclosing
        // context (spec 15.4).
        let saved = (parser.in_async, parser.in_generator, parser.top_level_await);
        parser.in_async = is_async;
        parser.in_generator = false;
        parser.top_level_await = false;
        let expr = parse_assignment(parser, true);
        (parser.in_async, parser.in_generator, parser.top_level_await) = saved;
        Ok(ArrowBody::Expr(Box::new(expr?)))
    }
}
