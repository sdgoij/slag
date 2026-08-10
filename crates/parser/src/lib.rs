//! Recursive-descent parser: syntactic grammar, cover grammar, early errors,
//! and ASI (spec ch. 13-17).
//!
//! Phase 3 implements the parameterized grammar `[Yield, Await, Return, In]`
//! and the syntax-directed operations the evaluator needs.

mod expr;
mod parser;
mod stmt;
mod token_stream;

use crux::JsError;
use syntax::{Program, SourceText};

pub use parser::Parser;

/// Parses a Script (spec 16.1): a statement list with Annex B HTML comments
/// enabled.
pub fn parse_script(source: &str) -> Result<Program, JsError> {
    let source = SourceText::from_utf8(source);
    let mut parser = Parser::new(&source, true);
    let strict = expr::scan_directive_prologue(&mut parser)?;
    parser.strict = strict;
    let body = stmt::parse_statement_list(&mut parser, syntax::TokenKind::Eof)?;
    // The script must be fully consumed.
    let tok = parser.peek()?.clone();
    if tok.kind != syntax::TokenKind::Eof {
        return Err(parser.unexpected(&tok));
    }
    let span = crux::Span::new(0, source.len() as u32);
    Ok(Program { body, span })
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntax::{BinaryOp, Expr, ExprKind, Literal, LogicalOp, StmtKind, UnaryOp};

    fn ok(source: &str) -> Program {
        parse_script(source)
            .unwrap_or_else(|e| panic!("expected {source:?} to parse: {e} at {:?}", e.span))
    }

    fn err(source: &str) {
        assert!(
            parse_script(source).is_err(),
            "expected {source:?} to be a syntax error"
        );
    }

    fn stmt(source: &str) -> syntax::Stmt {
        let program = ok(source);
        assert_eq!(
            program.body.len(),
            1,
            "expected one statement in {source:?}"
        );
        program.body.into_iter().next().unwrap()
    }

    fn expr_stmt(source: &str) -> Expr {
        match stmt(source).kind {
            StmtKind::Expr(expr) => expr,
            other => panic!("expected expression statement in {source:?}, got {other:?}"),
        }
    }

    fn binary(source: &str) -> (BinaryOp, Expr, Expr) {
        match expr_stmt(source).kind {
            ExprKind::Binary { op, left, right } => (op, *left, *right),
            other => panic!("expected binary expression in {source:?}, got {other:?}"),
        }
    }

    fn binary_expr(expr: &Expr) -> (BinaryOp, Expr, Expr) {
        match &expr.kind {
            ExprKind::Binary { op, left, right } => (*op, *left.clone(), *right.clone()),
            other => panic!("expected binary expression, got {other:?}"),
        }
    }

    #[test]
    fn parses_literals_and_identifiers() {
        assert!(matches!(
            expr_stmt("42").kind,
            ExprKind::Literal(Literal::Number(42.0))
        ));
        assert!(matches!(
            expr_stmt("3.5").kind,
            ExprKind::Literal(Literal::Number(3.5))
        ));
        assert!(matches!(
            expr_stmt("'hi'").kind,
            ExprKind::Literal(Literal::Str(_))
        ));
        assert!(matches!(
            expr_stmt("true").kind,
            ExprKind::Literal(Literal::Boolean(true))
        ));
        assert!(matches!(
            expr_stmt("null").kind,
            ExprKind::Literal(Literal::Null)
        ));
        assert!(matches!(expr_stmt("x").kind, ExprKind::Ident(_)));
        assert!(matches!(expr_stmt("this").kind, ExprKind::This));
        assert!(matches!(
            expr_stmt("/ab+/i").kind,
            ExprKind::Literal(Literal::RegExp { .. })
        ));
        assert!(matches!(
            expr_stmt("123n").kind,
            ExprKind::Literal(Literal::BigInt(_))
        ));
    }

    #[test]
    fn parses_operators_with_precedence() {
        // 1 + 2 * 3 = 1 + (2 * 3)
        let (op, left, right) = binary("1 + 2 * 3");
        assert_eq!(op, BinaryOp::Add);
        assert!(matches!(left.kind, ExprKind::Literal(Literal::Number(1.0))));
        let (op2, _, _) = binary_expr(&right);
        assert_eq!(op2, BinaryOp::Mul);

        // a - b - c is left-associative
        let (op, left, _) = binary("a - b - c");
        assert_eq!(op, BinaryOp::Sub);
        assert!(matches!(
            left.kind,
            ExprKind::Binary {
                op: BinaryOp::Sub,
                ..
            }
        ));

        // 2 ** 3 ** 2 is right-associative
        let (_, _, right) = binary("2 ** 3 ** 2");
        assert!(matches!(
            right.kind,
            ExprKind::Binary {
                op: BinaryOp::Exp,
                ..
            }
        ));
    }

    #[test]
    fn parses_logical_and_nullish_mixing_rules() {
        let expr = expr_stmt("a && b || c");
        assert!(matches!(
            expr.kind,
            ExprKind::Logical {
                op: LogicalOp::Or,
                ..
            }
        ));
        let expr = expr_stmt("a ?? b");
        assert!(matches!(
            expr.kind,
            ExprKind::Logical {
                op: LogicalOp::Nullish,
                ..
            }
        ));
        // Mixing ?? with && / || without parens is a syntax error.
        err("a ?? b || c");
        err("a || b ?? c");
        err("a && b ?? c");
        err("a ?? b && c");
        ok("(a ?? b) || c");
        ok("a ?? b | c");
    }

    #[test]
    fn exponentiation_base_restriction() {
        err("-x ** y");
        err("!x ** y");
        err("await x ** y");
        ok("(-x) ** y");
        ok("x++ ** y");
        ok("++x ** y");
        ok("x ** -y");
    }

    #[test]
    fn parses_unary_and_update() {
        assert!(matches!(
            expr_stmt("!x").kind,
            ExprKind::Unary {
                op: UnaryOp::Not,
                ..
            }
        ));
        assert!(matches!(
            expr_stmt("-x").kind,
            ExprKind::Unary {
                op: UnaryOp::Minus,
                ..
            }
        ));
        assert!(matches!(
            expr_stmt("typeof x").kind,
            ExprKind::Unary {
                op: UnaryOp::Typeof,
                ..
            }
        ));
        assert!(matches!(
            expr_stmt("x++").kind,
            ExprKind::Update { prefix: false, .. }
        ));
        assert!(matches!(
            expr_stmt("--x").kind,
            ExprKind::Update { prefix: true, .. }
        ));
        // Postfix ++ cannot span a line break.
        err("x\n++");
    }

    #[test]
    fn parses_assignment_and_compound() {
        let expr = expr_stmt("a = 1");
        assert!(matches!(
            expr.kind,
            ExprKind::Assign {
                op: syntax::AssignOp::Assign,
                ..
            }
        ));
        assert!(matches!(
            expr_stmt("a += 2").kind,
            ExprKind::Assign {
                op: syntax::AssignOp::AddAssign,
                ..
            }
        ));
        assert!(matches!(
            expr_stmt("a ??= 2").kind,
            ExprKind::Assign {
                op: syntax::AssignOp::NullishAssign,
                ..
            }
        ));
        // Invalid targets.
        err("1 = 2");
        err("a + b = c");
        err("a?.b = c");
        err("a?.b++");
        // Valid targets.
        ok("(a) = 5");
        ok("a.b = 5");
        ok("[a, b] = c");
        ok("({a} = c)");
    }

    #[test]
    fn parses_member_and_call_chains() {
        let expr = expr_stmt("a.b.c");
        assert!(matches!(
            expr.kind,
            ExprKind::Member(syntax::MemberExpr { .. })
        ));
        let expr = expr_stmt("f(1, 2)");
        assert!(matches!(expr.kind, ExprKind::Call(c) if c.args.len() == 2));
        let expr = expr_stmt("f(...args)");
        assert!(matches!(
            expr.kind,
            ExprKind::Call(c) if matches!(c.args[0], syntax::Argument::Spread(_))
        ));
        // Optional chaining.
        let expr = expr_stmt("a?.b");
        assert!(matches!(
            expr.kind,
            ExprKind::Member(syntax::MemberExpr { optional: true, .. })
        ));
        let expr = expr_stmt("a?.b.c");
        assert!(matches!(
            expr.kind,
            ExprKind::Member(syntax::MemberExpr {
                optional: false,
                object,
                ..
            }) if matches!(object.kind, ExprKind::Member(syntax::MemberExpr { optional: true, .. }))
        ));
        let expr = expr_stmt("a?.(1)");
        assert!(matches!(
            expr.kind,
            ExprKind::Call(c) if c.optional
        ));
        err("new a?.b()");
        err("a?.b`x`");
        ok("new a.b()");
        ok("new (a.b)()");
    }

    #[test]
    fn parses_new_expressions() {
        let expr = expr_stmt("new Foo()");
        let ExprKind::New(n) = expr.kind else {
            panic!("expected new expression")
        };
        assert!(n.args.is_empty());
        assert!(matches!(
            n.callee.kind,
            ExprKind::Ident(ref atom) if atom == &crux::intern_utf8("Foo")
        ));
        let expr = expr_stmt("new Foo(1)");
        assert!(matches!(expr.kind, ExprKind::New(n) if n.args.len() == 1));
        let expr = expr_stmt("new Foo");
        assert!(matches!(expr.kind, ExprKind::New(n) if n.args.is_empty()));
        // new.target
        let expr = expr_stmt("new.target");
        assert!(matches!(
            expr.kind,
            ExprKind::MetaProperty { ref meta, .. } if meta == &crux::intern_utf8("new")
        ));
    }

    #[test]
    fn parses_arrays_and_objects() {
        let expr = expr_stmt("[1, , 2]");
        assert!(matches!(
            expr.kind,
            ExprKind::Array(a)
                if a.elements.len() == 3
                    && matches!(a.elements[1], syntax::ArrayElement::Hole)
        ));
        let expr = expr_stmt("[...x]");
        assert!(matches!(
            expr.kind,
            ExprKind::Array(a) if matches!(a.elements[0], syntax::ArrayElement::Spread(_))
        ));
        let expr = expr_stmt("({a: 1, b})");
        assert!(matches!(
            expr.kind,
            ExprKind::Paren(inner)
                if matches!(inner.kind, ExprKind::Object(ref o) if o.props.len() == 2)
        ));
        let expr = expr_stmt("({a, b = 2} = c)");
        assert!(matches!(
            expr.kind,
            ExprKind::Paren(inner) if matches!(inner.kind, ExprKind::Assign { .. })
        ));
        // CoverInitializedName outside a pattern context is an error.
        err("x = {a = 1}");
        err("f({a = 1})");
        err("({a = 1})");
        ok("({a = 1}) => x");
        ok("({a: b = 1}) => x");
        // Methods and accessors.
        ok("({ m() {} })");
        ok("({ get x() { return 1; } })");
        ok("({ set x(v) {} })");
        ok("({ async m() {} })");
        ok("({ *gen() {} })");
        ok("({ async *gen() {} })");
        // `async` as a normal property name.
        ok("({ async: 1 })");
        ok("({ async() {} })");
    }

    #[test]
    fn parses_arrow_functions() {
        let expr = expr_stmt("x => x + 1");
        assert!(matches!(
            expr.kind,
            ExprKind::Arrow {
                is_async: false,
                ..
            }
        ));
        let expr = expr_stmt("(a, b) => a");
        assert!(matches!(expr.kind, ExprKind::Arrow { .. }));
        let expr = expr_stmt("() => {}");
        assert!(matches!(expr.kind, ExprKind::Arrow { .. }));
        let expr = expr_stmt("(a = 1) => a");
        assert!(matches!(expr.kind, ExprKind::Arrow { .. }));
        let expr = expr_stmt("([a, b]) => a");
        assert!(matches!(expr.kind, ExprKind::Arrow { .. }));
        let expr = expr_stmt("({a, b = 1}) => a");
        assert!(matches!(expr.kind, ExprKind::Arrow { .. }));
        let expr = expr_stmt("(...rest) => rest");
        assert!(matches!(expr.kind, ExprKind::Arrow { .. }));
        ok("async x => x");
        ok("async (x, y) => x");
        ok("async () => {}");
        // Parenthesized expression vs arrow.
        assert!(matches!(expr_stmt("(a, b)").kind, ExprKind::Paren(_)));
        // Rest must be last; bad covers.
        err("(...a, b) => x");
        err("a + b => x");
        err("(a,)");
        err("(...a)");
        err("(1) => x");
    }

    #[test]
    fn parses_templates() {
        let expr = expr_stmt("`hello`");
        assert!(matches!(
            expr.kind,
            ExprKind::Template(t) if t.quasis.len() == 1 && t.exprs.is_empty()
        ));
        let expr = expr_stmt("`a${b}c${d}e`");
        assert!(matches!(
            expr.kind,
            ExprKind::Template(t) if t.quasis.len() == 3 && t.exprs.len() == 2
        ));
        let expr = expr_stmt("tag`a${b}c`");
        assert!(matches!(expr.kind, ExprKind::TaggedTemplate { .. }));
        // Template substitution expressions and nesting.
        ok("`${`nested${x}`}`");
        ok("`${1 + 2}`");
    }

    #[test]
    fn division_vs_regexp_goal() {
        assert!(matches!(
            expr_stmt("a / b").kind,
            ExprKind::Binary {
                op: BinaryOp::Div,
                ..
            }
        ));
        assert!(matches!(
            expr_stmt("a / b / c").kind,
            ExprKind::Binary {
                op: BinaryOp::Div,
                ..
            }
        ));
        // After an operator, / starts a regexp.
        assert!(matches!(
            expr_stmt("typeof /x/").kind,
            ExprKind::Unary {
                op: UnaryOp::Typeof,
                ..
            }
        ));
        ok("x = /y/");
        ok("f(/y/)");
        ok("function f() { return /y/; }");
    }

    #[test]
    fn parses_statements() {
        assert!(matches!(stmt("{ }").kind, StmtKind::Block(_)));
        assert!(matches!(stmt(";").kind, StmtKind::Empty));
        assert!(matches!(stmt("x;").kind, StmtKind::Expr(_)));
        assert!(matches!(
            stmt("if (a) b; else c;").kind,
            StmtKind::If { .. }
        ));
        assert!(matches!(
            stmt("if (a) b;").kind,
            StmtKind::If {
                alternate: None,
                ..
            }
        ));
        assert!(matches!(
            stmt("if (a) b; else if (c) d;").kind,
            StmtKind::If {
                alternate: Some(_),
                ..
            }
        ));
        assert!(matches!(stmt("while (a) b;").kind, StmtKind::While { .. }));
        assert!(matches!(
            stmt("do b; while (a);").kind,
            StmtKind::DoWhile { .. }
        ));
        assert!(matches!(stmt("for (;;) {}").kind, StmtKind::For { .. }));
        assert!(matches!(
            stmt("for (var i = 0; i < 10; i++) {}").kind,
            StmtKind::For { .. }
        ));
        assert!(matches!(
            stmt("for (var k in obj) {}").kind,
            StmtKind::ForIn { .. }
        ));
        assert!(matches!(
            stmt("for (k of arr) {}").kind,
            StmtKind::ForOf { .. }
        ));
        assert!(matches!(
            stmt("for (let k of arr) {}").kind,
            StmtKind::ForOf { .. }
        ));
        ok("while (1) { break; }");
        ok("for (;;) { continue; }");
        ok("do { break; } while (0);");
        assert!(matches!(stmt("debugger;").kind, StmtKind::Debugger));
        assert!(matches!(
            stmt("throw new Error();").kind,
            StmtKind::Throw(_)
        ));
        assert!(matches!(
            stmt("try {} catch (e) {} finally {}").kind,
            StmtKind::Try {
                handler: Some(_),
                finalizer: Some(_),
                ..
            }
        ));
        assert!(matches!(
            stmt("try {} finally {}").kind,
            StmtKind::Try { handler: None, .. }
        ));
        assert!(matches!(stmt("try {} catch {}").kind, StmtKind::Try { .. }));
        assert!(matches!(
            stmt("switch (x) { case 1: a; break; default: b; }").kind,
            StmtKind::Switch { .. }
        ));
        assert!(matches!(
            stmt("label: for (;;) break label;").kind,
            StmtKind::Labeled { .. }
        ));
        assert!(matches!(stmt("with (x) y;").kind, StmtKind::With { .. }));
        assert!(matches!(
            stmt("var a = 1, b = 2;").kind,
            StmtKind::VarDecl { decls, .. } if decls.len() == 2
        ));
        assert!(matches!(
            stmt("let a;").kind,
            StmtKind::VarDecl {
                kind: syntax::VarDeclKind::Let,
                ..
            }
        ));
        assert!(matches!(
            stmt("const a = 1;").kind,
            StmtKind::VarDecl {
                kind: syntax::VarDeclKind::Const,
                ..
            }
        ));
        assert!(matches!(
            stmt("function f() {}").kind,
            StmtKind::FunctionDecl(_)
        ));
        assert!(
            matches!(stmt("function* g() {}").kind, StmtKind::FunctionDecl(f) if f.is_generator)
        );
        assert!(
            matches!(stmt("async function h() {}").kind, StmtKind::FunctionDecl(f) if f.is_async)
        );
        assert!(matches!(stmt("let a = 1;").kind, StmtKind::VarDecl { .. }));
    }

    #[test]
    fn function_bodies_allow_return_and_blocks() {
        ok("function f() { return 1; }");
        ok("function f() { var x = 1; return x; }");
        ok("function f(a, b = 2) { return a + b; }");
        ok("function f({a, b}, [c]) { return a; }");
        ok("function f(...rest) { return rest; }");
        ok("(function() { return 1; })");
        ok("(function named() {})");
        // Duplicate params in sloppy simple functions are allowed.
        ok("function f(a, a) {}");
        err("'use strict'; function f(a, a) {}");
        err("function f(a = 1, a) {}");
        // Non-simple params cannot be combined with a use-strict directive.
        err("function f(a = 1) { 'use strict'; }");
        // return outside a function.
        err("return 1;");
    }

    #[test]
    fn asi_rules() {
        // Newline, }, and EOF all trigger ASI.
        ok("a\nb");
        ok("{ a }");
        ok("a");
        // return's argument cannot span a newline.
        let program = ok("function f() { return\n1; }");
        let StmtKind::FunctionDecl(f) = &program.body[0].kind else {
            panic!("expected function")
        };
        assert!(matches!(f.body.stmts[0].kind, StmtKind::Return(None)));
        // throw always needs an expression on the same line.
        err("throw\nnew Error();");
        // break/continue labels cannot span a newline.
        ok("for (;;) { break\nx; }");
        // Restricted ++/-- postfix.
        ok("a\n++b");
        // Division across lines: a / b / c parses as a chain.
        let program = ok("a\n/ b\n/ c");
        assert!(matches!(program.body[0].kind, StmtKind::Expr(_)));
    }

    #[test]
    fn early_errors() {
        err("let x; let x;");
        err("let x; var x;");
        err("var x; let x;");
        err("const x = 1; const x = 2;");
        err("function f() { let x; let x; }");
        ok("var x; var x;");
        ok("var x; { let x; }");
        err("function f() { let x; { var x; } }");
        // Strict-mode restrictions.
        err("'use strict'; with (x) {}");
        err("'use strict'; delete x;");
        err("'use strict'; let eval = 1;");
        err("'use strict'; var arguments;");
        err("function f() { 'use strict'; let arguments; }");
        // yield/await contexts.
        err("function* g() { let yield; }");
        // break/continue outside loops.
        err("break;");
        err("continue;");
        err("label: { continue label; }");
        ok("label: for (;;) { continue label; }");
        err("for (;;) { continue missing; }");
        // Duplicate function declarations are strict-mode errors.
        ok("function f() {} function f() {}");
        err("'use strict'; function f() {} function f() {}");
        // for-in/of restrictions.
        err("for (var x = 0 in obj) {}");
        err("for (const x of arr) {}");
        // Missing const initializer.
        err("const x;");
        err("for (const x; ;) {}");
    }

    #[test]
    fn statements_that_are_not_expressions() {
        // `{` and `function` cannot start an expression statement; here `{ }`
        // is a block followed by a unary `+1` statement.
        ok("{ } + 1");
        // let [ is a declaration, not an expression.
        ok("let [a] = b;");
    }

    #[test]
    fn parses_conditional_and_sequence() {
        let expr = expr_stmt("a ? b : c");
        assert!(matches!(expr.kind, ExprKind::Conditional { .. }));
        let expr = expr_stmt("(a, b, c)");
        assert!(matches!(expr.kind, ExprKind::Paren(_)));
        // The conditional middle is an assignment, so a comma cannot bind
        // there (spec 13.14).
        err("a ? b, c : d");
        ok("a ? (b, c) : d");
    }

    #[test]
    fn parses_destructuring_declarations() {
        ok("var [a, b] = c;");
        ok("var [a, , b] = c;");
        ok("var [a = 1] = c;");
        ok("var [...rest] = c;");
        ok("var {a, b: c} = d;");
        ok("var {a = 1} = d;");
        ok("var {a: {b}} = d;");
        ok("var {...rest} = d;");
        ok("var [a, ...rest] = c;");
        err("var [...a, b] = c;");
        err("var {a, ...rest, b} = d;");
    }

    #[test]
    fn parses_import_call_and_meta() {
        ok("import('x')");
        ok("import('x', { with: { type: 'json' } })");
        // import.meta is parseable as an expression in scripts too.
        ok("import.meta");
    }

    #[test]
    fn invalid_sources_error() {
        err("a b");
        err("if (a)");
        err("(a, b");
        err("a =");
        err("a +");
        err("function () {}");
        err("try {}");
        err("`unterminated");
        err("f(a, b");
        err("1 2");
        err("a b c");
    }

    #[test]
    fn html_comments_allowed_in_scripts() {
        ok("<!-- x\n--> y");
    }

    #[test]
    fn spans_cover_source() {
        let program = ok("var x = 42;");
        assert_eq!(program.span, crux::Span::new(0, 11));
        let s = stmt("let x = 1;");
        assert_eq!(s.span, crux::Span::new(0, 10));
    }

    #[test]
    fn nested_arrow_cover_grammar() {
        ok("(({a = 1}) => x)");
        err("(({a = 1})) => x");
        err("(({a = 1}), b) => x");
        ok("({a = 1}, b) => x");
        err("(a, (b) => c) => d");
        ok("f(({a = 1}) => a)");
    }
}
