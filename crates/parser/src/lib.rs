//! Recursive-descent parser: syntactic grammar, cover grammar, early errors,
//! and ASI (spec ch. 13-17).
//!
//! Phase 3 implements the parameterized grammar `[Yield, Await, Return, In]`
//! and the syntax-directed operations the evaluator needs.

mod class;
mod early_errors;
mod expr;
mod module;
mod parser;
mod stmt;
mod token_stream;

use crux::JsError;
use syntax::{Module, Program, SourceText};

pub use parser::Parser;

/// Parses a Script (spec 16.1): a statement list with Annex B HTML comments
/// enabled.
pub fn parse_script(source: &str) -> Result<Program, JsError> {
    parse_script_units(
        &source.encode_utf16().collect::<Vec<u16>>(),
        false,
        None,
        &[],
    )
}

/// Like `parse_script`, for UTF-16 source text (the `eval` path, where the
/// code is a `JsString` and lone surrogates must survive intact). The eval
/// goal tolerates a HashbangComment after a leading directive prologue: the
/// runtime's strict-eval validation re-parses the source with a synthetic
/// `'use strict';` prefix, which would otherwise push a leading hashbang off
/// the first position.
pub fn parse_script_utf16(units: &[u16]) -> Result<Program, JsError> {
    parse_script_units(units, true, None, &[])
}

/// The eval-caller context that relaxes the Script early errors for a direct
/// eval (spec 19.2.1.1): `new.target` and `super` are legal in eval code
/// when the caller is inside function/method code, and PrivateIdentifiers
/// parse when the caller has a private environment (the runtime resolves
/// them against the inherited private names).
#[derive(Debug, Clone, Copy)]
pub struct EvalContext {
    /// The caller is inside a non-arrow function: `new.target` is valid.
    pub in_function: bool,
    /// The caller is inside a method: `super` property access is valid.
    pub in_method: bool,
    /// The caller has a private environment: `#name` is valid.
    pub allow_private: bool,
}

/// Like `parse_script_utf16`, for eval code whose early errors depend on the
/// caller's context (spec 19.2.1.1 steps 5-7). `caller_private_names` are the
/// caller's private identifiers (without the `#`), used to validate eval'd
/// `#name` uses against the inherited private environment.
pub fn parse_script_utf16_eval(
    units: &[u16],
    eval: &EvalContext,
    caller_private_names: &[crux::AtomId],
) -> Result<Program, JsError> {
    parse_script_units(units, true, Some(*eval), caller_private_names)
}

fn parse_script_units(
    units: &[u16],
    allow_hashbang_after_directives: bool,
    eval: Option<EvalContext>,
    caller_private_names: &[crux::AtomId],
) -> Result<Program, JsError> {
    let source = SourceText::from_utf16(units.to_vec());
    let mut parser = Parser::new(&source, true, allow_hashbang_after_directives);
    if let Some(eval) = eval {
        // The eval goal inherits the caller's function/method context: the
        // Script early errors for `new.target`/`super` are relaxed, and a
        // caller inside a class makes PrivateIdentifiers parse (their
        // declaration check runs against the caller's names below).
        parser.nt_context = eval.in_function;
        parser.allow_super = eval.in_method;
        if eval.allow_private {
            let mut names = std::collections::HashMap::new();
            for name in caller_private_names {
                names.insert(*name, Default::default());
            }
            parser.private_names.push(names);
        }
    }
    let strict = expr::scan_directive_prologue(&mut parser)?;
    parser.strict = strict;
    let body = stmt::parse_statement_list(&mut parser, syntax::TokenKind::Eof)?;
    // The script must be fully consumed.
    let tok = parser.peek()?.clone();
    if tok.kind != syntax::TokenKind::Eof {
        return Err(parser.unexpected(&tok));
    }
    let span = crux::Span::new(0, source.len() as u32);
    let program = Program { body, span };
    match eval {
        Some(_) => early_errors::check_script_eval(&program, caller_private_names)?,
        None => early_errors::check_script(&program)?,
    }
    Ok(program)
}

/// Parses a Module (spec 16.2): import/export declarations plus a strict
/// statement list. Modules are always strict, `await` is reserved, top-level
/// `await` is allowed, and HTML comments are rejected.
pub fn parse_module(source: &str) -> Result<Module, JsError> {
    let source = SourceText::from_utf8(source);
    let mut parser = Parser::new(&source, false, false);
    parser.in_module = true;
    parser.strict = true;
    parser.top_level_await = true;
    let body = module::parse_module_items(&mut parser)?;
    // The module must be fully consumed.
    let tok = parser.peek()?.clone();
    if tok.kind != syntax::TokenKind::Eof {
        return Err(parser.unexpected(&tok));
    }
    let span = crux::Span::new(0, source.len() as u32);
    let module = Module { body, span };
    early_errors::check_module(&module)?;
    Ok(module)
}

/// Parses a FunctionExpression (spec 15.2.4): the source `CreateDynamicFunction`
/// assembles for `new Function(...)`. The expression must be fully consumed and
/// the function's early errors apply.
pub fn parse_function(source: &str) -> Result<syntax::ast::Function, JsError> {
    parse_function_with_async(source, false)
}

/// Like `parse_function`, for the `async function` / `async function*` forms
/// CreateDynamicFunction assembles for the AsyncFunction and
/// AsyncGeneratorFunction constructors; `is_async` consumes the leading
/// `async` keyword.
pub fn parse_function_with_async(
    source: &str,
    is_async: bool,
) -> Result<syntax::ast::Function, JsError> {
    let source = SourceText::from_utf8(source);
    let mut parser = Parser::new(&source, true, false);
    if is_async {
        parser.next()?; // `async`
    }
    let expr = expr::parse_function_expression(&mut parser, is_async)?;
    let tok = parser.peek()?.clone();
    if tok.kind != syntax::TokenKind::Eof {
        return Err(parser.unexpected(&tok));
    }
    let syntax::ExprKind::Function(function) = expr.kind else {
        unreachable!("function expression");
    };
    early_errors::check_function(&function)?;
    Ok(function)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crux::JsString;
    use syntax::{
        AttributeKey, BinaryOp, ExportDecl, ExportName, ExportSpecifier, Expr, ExprKind,
        ForBinding, ForInit, ImportEntry, Literal, LogicalOp, ModuleItem, StmtKind, UnaryOp,
        VarDeclKind,
    };

    fn ok(source: &str) -> Program {
        parse_script(source)
            .unwrap_or_else(|e| panic!("expected {source:?} to parse: {e} at {:?}", e.span))
    }

    #[test]
    fn html_close_comment_with_leading_whitespace() {
        ok("--> a comment\nthrow 1;");
        ok("  --> a comment\nthrow 1;");
        ok("/* c */ --> a comment\nthrow 1;");
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
        // new.target — legal only inside a function body.
        let program = ok("function f() { new.target; }");
        let StmtKind::FunctionDecl(f) = &program.body[0].kind else {
            panic!("expected function");
        };
        let expr = match &f.body.stmts[0].kind {
            StmtKind::Expr(e) => e.clone(),
            other => panic!("expected expression statement, got {other:?}"),
        };
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
    fn template_invalid_escapes_are_early_errors() {
        // An untagged template whose TV is undefined (an invalid escape
        // sequence) is a SyntaxError (spec 13.2.8.1); tagged templates are
        // exempt.
        err("`\\x0`;");
        err("`\\x1`;");
        err("`\\u0`;");
        err("`\\u`;");
        err("`\\u{1F_639}`;");
        err("`\\00`;");
        err("`\\8`;");
        err("`\\9`;");
        err("`a${b}\\x1`;");
        ok("tag`\\x0`;");
        ok("`a${tag`\\x0`}b`;");
        ok("`a\\nb`;");
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
        // A "use strict" directive makes the parameter list strict too.
        err("function f(eval) { 'use strict'; }");
        err("function f(a, a) { 'use strict'; }");
        err("(a, a) => { 'use strict'; }");
        // yield/await contexts.
        err("function* g() { let yield; }");
        // break/continue outside loops.
        err("break;");
        err("continue;");
        err("label: { continue label; }");
        ok("label: for (;;) { continue label; }");
        err("for (;;) { continue missing; }");
        // Duplicate function declarations are allowed in both modes (they
        // are var-scoped, spec 16.1.2); a lexical redeclaration still errors.
        ok("function f() {} function f() {}");
        ok("'use strict'; function f() {} function f() {}");
        err("let f; function f() {}");
        err("function f() {} let f;");
        // for-in/of restrictions.
        // Annex B.2.6 allows `for (var x = 0 in obj)` in sloppy code.
        ok("for (var x = 0 in obj) {}");
        err("'use strict'; for (var x = 0 in obj) {}");
        err("for (let x = 0 in obj) {}");
        err("for (var x = 0 of arr) {}");
        // for-of heads allow bindings without initializers.
        ok("for (const x of arr) {}");
        ok("for (let [a, b] of arr) {}");
        // Missing const initializer.
        err("const x;");
        err("for (const x; ;) {}");
    }

    #[test]
    fn escaped_keywords_are_not_keywords() {
        // Terminal symbols must appear exactly as written (spec 5.1.5): an
        // escaped keyword is an IdentifierName, and as an expression it is an
        // invalid IdentifierReference (a reserved word).
        err("f\\u{61}lse;");
        err("tru\\u{65};");
        err("n\\u{75}ll;");
        err("n\\u0065w.target;");
        err("im\\u0070ort('./x.js');");
        err("t\\u0068is;");
        // Escaped non-keywords are ordinary identifiers.
        ok("\\u0061bc;");
        ok("var o = { \\u0074rue: 1 };");
        ok("obj.\\u0074rue;");
    }

    #[test]
    fn async_params_reject_await_and_yield() {
        // Async and async-generator formal parameters must not contain an
        // AwaitExpression; generators reject YieldExpression (spec 15.5.1).
        err("(async function*(x = await 1) { });");
        err("(async function(x = await 1) { });");
        err("(function*(x = yield 1) { });");
        err("async function f(x = await 1) { }");
        err("async function* g(x = await 1) { }");
        // Outside an async function `await` is a plain identifier, so
        // `await 1` cannot parse; it is only usable as a bare name.
        err("function f(x = await 1) { }");
        ok("function f(x = await) { }");
        ok("(async function*(x = 1) { });");
    }

    #[test]
    fn parenthesized_import_call_in_new_is_valid() {
        // `new (import(…))` is a NewExpression whose callee is a
        // PrimaryExpression (the parenthesized cover grammar); only the
        // direct `new import(…)` form is excluded (spec 13.3.4).
        ok("new (import(''));");
        ok("new (function() {}, import(''));");
        ok("typeof new (import(''), function() {});");
        err("new import('');");
    }

    #[test]
    fn for_of_let_lhs_lookahead() {
        // `for (let of …)` cannot be an expression-headed for-of (the
        // `[lookahead ≠ let]` restriction), and `let of` as a ForDeclaration
        // leaves no `of` keyword (spec 14.7.5).
        err("for (let of []) {}");
        err("for (let of x) {}");
        err("for (const of []) {}");
        // `let of` binding with the `of` keyword after it is a declaration.
        ok("for (let of of []) {}");
        ok("for (let of in x) {}");
        ok("let of; for (of of []) {}");
        ok("for (let of = 1; ; ) {}");
        ok("for (let x of []) {}");
    }

    #[test]
    fn static_block_contains_await_skips_arrow_bodies() {
        // The static-block early error is `Contains await` (spec 15.7.11):
        // arrows are opaque to it (spec sec-static-semantics-contains), so
        // `await` in an arrow body is fine while a direct reference is not.
        ok("class C { static { (() => ({ await })); } }");
        ok("class C { static { (async () => await 1); } }");
        ok("class C { static { (() => await); } }");
        // Arrow parameters inherit [+Await] and the arrow's own early error
        // rejects an AwaitExpression there.
        err("class C { static { (x = await 1) => {}; } }");
        err("class C { static { ({ await }); } }");
        err("class C { static { await 1; } }");
        // `arguments` still counts inside arrow bodies (ContainsArguments
        // recurses through arrows).
        err("class C { static { (() => arguments); } }");
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
    fn parses_classes() {
        // Declarations, expressions, heritage, and empty bodies.
        assert!(matches!(stmt("class A {}").kind, StmtKind::ClassDecl(c) if c.name.is_some()));
        assert!(matches!(
            expr_stmt("(class A {})").kind,
            ExprKind::Paren(inner)
                if matches!(inner.kind, ExprKind::Class(ref c) if c.name.is_some())
        ));
        assert!(matches!(
            expr_stmt("(class {})").kind,
            ExprKind::Paren(inner)
                if matches!(inner.kind, ExprKind::Class(ref c) if c.name.is_none())
        ));
        assert!(matches!(
            stmt("class A extends B {}").kind,
            StmtKind::ClassDecl(c) if c.heritage.is_some()
        ));

        // Methods of every kind, static variants, fields, and static blocks.
        ok("class A { m() {} }");
        ok("class A { static m() {} }");
        ok("class A { get x() { return 1; } set x(v) {} }");
        ok("class A { static get x() { return 1; } }");
        ok("class A { *gen() {} }");
        ok("class A { static *gen() {} }");
        ok("class A { async m() {} }");
        ok("class A { async *gen() {} }");
        ok("class A { x = 1; }");
        ok("class A { x = 1 }");
        ok("class A { static x = 1; y = 2; }");
        ok("class A { [expr] = 1; }");
        ok("class A { #priv = 1; }");
        ok("class A { static #priv; }");
        ok("class A { static {} }");
        ok("class A { ; }");
        ok("class A { ; m() {} }");
        // `static` as a field or method name.
        ok("class A { static = 1; }");
        ok("class A { static() {} }");

        // Constructor forms.
        ok("class A { constructor() {} }");
        ok("class A { constructor(x) { this.x = x; } }");
        ok("class A { constructor() {} static constructor() {} }");

        // super in methods and constructors.
        ok("class A extends B { m() { return super.x; } }");
        ok("class A extends B { m() { return super[0]; } }");
        ok("class A extends B { constructor() { super(); } }");
        ok("class A extends B { x = super.y; }");
        ok("class A extends B { static { super.z; } }");
        // Arrows capture super.
        ok("class A extends B { m() { return () => super.x; } }");
        // Private methods and accessor pairs.
        ok("class A { #m() {} }");
        ok("class A { static #m() {} }");
        ok("class A { get #x() {} set #x(v) {} }");
        ok("class A { static get #x() {} static set #x(v) {} }");
    }

    #[test]
    fn class_early_errors() {
        // A class declaration requires a name.
        err("class {};");
        // Duplicate constructors and special-method constructors.
        err("class A { constructor() {} constructor() {} }");
        err("class A { get constructor() {} }");
        err("class A { async constructor() {} }");
        err("class A { *constructor() {} }");
        // Fields named constructor / prototype.
        err("class A { constructor = 1; }");
        err("class A { static prototype = 1; }");
        err("class A { static constructor = 1; }");
        err("class A { static prototype() {} }");
        // Private-name rules.
        err("class A { #x; #x; }");
        err("class A { #x() {} #x() {} }");
        err("class A { #x; get #x() {} }");
        err("class A { get #x() {} set #x(v) {} #x; }");
        err("class A { #constructor; }");
        // A getter/setter pair must share static-ness.
        err("class A { static get #x() {} set #x(v) {} }");
        // super outside methods.
        err("super.x;");
        err("class A { m() { return () => super(); } }");
        // Class bodies are strict: with is illegal.
        err("class A { m() { with (x) {} } }");
        // Redeclaration rules apply to the class name.
        err("let A; class A {};");
        ok("class A {} let B;");
        // Class expressions may be anonymous and named.
        ok("const C = class {};");
        ok("const C = class Named {};");
    }

    #[test]
    fn class_name_is_always_strict() {
        // A class definition is strict mode code (spec 15.7.3), so the
        // strict-reserved words are not valid class names, escaped or not.
        err("class let {}");
        err("class static {}");
        err("class yield {}");
        err("class l\\u0065t {}");
        err("class st\\u0061tic {}");
        err("var C = class let {};");
        ok("class await {}");
        // Escaped contextual keywords are ordinary identifiers.
        err("class C { st\\u0061tic m() {} }");
        err("class C { \\u0061sync m() {} }");
        ok("class C { static m() {} }");
    }

    #[test]
    fn function_names_and_strict_bodies() {
        // A strict body (enclosing or directive) forbids eval/arguments names
        // (spec 15.4.1); sloppy bodies allow them.
        err("(function eval() { 'use strict'; });");
        err("(function arguments() { 'use strict'; });");
        err("'use strict'; (function eval() {});");
        ok("(function eval() {});");
        ok("function eval() {}");
        err("function eval() { 'use strict'; }");
        err("function arguments() { 'use strict'; }");
        // FunctionExpression names parse with [~Yield, ~Await]: `yield` and
        // `await` are ordinary names even in resumable code, while generator
        // and async-generator expression names keep their restrictions.
        ok("function* g() { (function yield() {}); }");
        ok("function* g() { (function await() {}); }");
        ok("class C { static { (function* await(await) {}); } }");
        err("var g = function* yield() {};");
        err("(async function* yield() {});");
        err("(async function* await() {});");
        ok("(async function await() {});");
        // Async function/method formals reserve `await` (spec 15.8.1).
        err("async function foo(await) {}");
        err("async function foo(x = await) {}");
        err("class A { async m(await) {} }");
        err("(async function*(await) {});");
    }

    #[test]
    fn yield_grammar_rules() {
        // A yield operand in a for-head parses with [~In] (spec 14.4.1).
        err("function* g() { for (yield '' in {}; ; ) ; }");
        err("function* g() { for (yield * '' in {}; ; ) ; }");
        ok("function* g() { yield '' in {}; }");
        // After `yield` the lexical goal is RegExp, so `/` starts a literal.
        ok("function* g() { received = yield/abc/i; }");
        // Arrow parameters may not contain a YieldExpression (spec 15.4.1).
        err("function* g() { (x = yield) => {}; }");
        err("async function f() { (x = await) => {}; }");
    }

    #[test]
    fn private_name_early_errors() {
        // A private reference must be declared in an enclosing class.
        err("class C { m() { this.#x; } }");
        err("class C { y = this.#x; }");
        err("class C { [this.#f] = 1; }");
        err("class C { f = (() => {})().#x; }");
        err("class C { m() { this.#x; class D extends C { #x; } } }");
        // A class's heritage cannot see the class's own private names.
        err("var C = class extends class { x = this.#foo; } { #foo; };");
        err("var C = class extends function() { x = this.#foo; } { #foo; };");
        // Forward references within a class are allowed.
        ok("class C { m() { return this.#x; } #x; }");
        // Duplicate private methods (all forms) are rejected.
        err("class C { #m() {} #m() {} }");
        err("class C { *#m() {} *#m() {} }");
        err("class C { async #m() {} async #m() {} }");
        err("class C { async *#m() {} async *#m() {} }");
    }

    #[test]
    fn delete_and_super_private_rules() {
        // delete of a private member is an early error (spec 13.6.2).
        err("class C { #x; m = delete this.#x; }");
        err("class C { #x; m = delete (g()).#x; }");
        // super may not access a private name (spec 15.7.10).
        err("class C { #m() {} m() { return super.#m; } }");
        // super() requires a derived class (spec 15.7.11).
        err("class C { constructor() { super(); } }");
        ok("class C extends B { constructor() { super(); } }");
    }

    #[test]
    fn private_in_grammar() {
        // `#name in ShiftExpression` (spec 13.11): the right operand must be
        // a ShiftExpression, so a nested private-in or a bare arrow function
        // at that level is a SyntaxError.
        err("class C { #f; m() { return #f in #f in this; } }");
        err("class C { #f; m() { return #f in () => {}; } }");
        err("class C { #f; m() { return #f in (x) => {}; } }");
        err("class C { #f; m() { return #f in async () => {}; } }");
        // Arrows nested inside a paren/arguments/literal are still valid.
        ok("class C { #f; m() { return #f in (() => {}); } }");
        ok("class C { #f; m() { return #f in (() => {})(); } }");
        ok("class C { #f; m() { return #f in f(() => {}); } }");
        ok("class C { #f; m() { return #f in new Foo(() => {}); } }");
        ok("class C { #f; m() { return #f in { m: () => {} }; } }");
        ok("class C { #f; m() { return #f in [() => {}]; } }");
        ok("class C { #f; m() { return #f in `x${() => {}}`; } }");
        ok("class C { #f; m() { return #f in (#g in this); } #g; }");
        // `#f in x` is a RelationalExpression and may be `in`-chained.
        ok("class C { #f; m(o) { return #f in o in this; } }");
    }

    #[test]
    fn setter_defaults_and_accessor_fields() {
        // A setter parameter may carry an initializer.
        ok("var C = class { set m(x = 1) {} };");
        // `accessor`-prefixed fields parse (decorators proposal).
        ok("var C = class { accessor $; accessor _; };");
        ok("var C = class { accessor \\u{6F}; };");
        // Without a following name-start it is a plain field.
        ok("var C = class { accessor = 1; };");
    }

    #[test]
    fn static_blocks_and_decorators() {
        // Static blocks are return-less.
        err("class C { static { return; } }");
        // Decorated classes and elements parse (syntax only).
        ok("@dec class C {};");
        ok("var C = @a.b @(c) class {};");
        ok("class C { @dec() m() {} }");
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
        // import.meta is only valid in module code (spec 16.2.1.5).
        err("import.meta");
        mod_ok("import.meta");
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
    fn eval_scripts_tolerate_hashbang_after_directive_prologue() {
        // A HashbangComment at position 0 is always fine.
        ok("#!x\n1");
        // The runtime's strict-eval validation re-parses the user's source
        // with a synthetic `'use strict';` prefix; the hashbang then follows
        // the directive prologue instead of sitting at position 0. The eval
        // entry point accepts that shape; a regular script does not.
        let units = "'use strict';\n#!x\n1".encode_utf16().collect::<Vec<u16>>();
        assert!(parse_script_utf16(&units).is_ok());
        assert!(parse_script("'use strict';\n#!x\n1").is_err());
        // The tolerance never accepts a hashbang after real code.
        let units = "x = 1;\n#!x\n2".encode_utf16().collect::<Vec<u16>>();
        assert!(parse_script_utf16(&units).is_err());
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

    // ---- modules (spec 16.2) ----

    fn mod_ok(source: &str) -> Module {
        parse_module(source)
            .unwrap_or_else(|e| panic!("expected {source:?} to parse: {e} at {:?}", e.span))
    }

    fn mod_err(source: &str) {
        assert!(
            parse_module(source).is_err(),
            "expected {source:?} to be a syntax error"
        );
    }

    #[test]
    fn parses_import_declarations() {
        // Default import.
        let m = mod_ok("import def from 'm';");
        assert_eq!(m.body.len(), 1);
        let ModuleItem::Import(imp) = &m.body[0] else {
            panic!("expected an import declaration");
        };
        assert_eq!(imp.specifier, JsString::from_utf8("m"));
        assert_eq!(imp.entries.len(), 1);
        assert!(matches!(
            &imp.entries[0],
            ImportEntry::Default { local, .. } if *local == crux::intern_utf8("def")
        ));
        assert!(imp.attributes.is_empty());

        // Default + named with aliases: the local and imported names differ.
        let m = mod_ok("import d, { a, b as c } from 'm';");
        let ModuleItem::Import(imp) = &m.body[0] else {
            panic!("expected an import declaration");
        };
        assert_eq!(imp.entries.len(), 3);
        assert!(matches!(&imp.entries[0], ImportEntry::Default { .. }));
        assert!(matches!(
            &imp.entries[1],
            ImportEntry::Named { imported: ExportName::Ident(a), local, .. }
                if *a == crux::intern_utf8("a") && *local == crux::intern_utf8("a")
        ));
        assert!(matches!(
            &imp.entries[2],
            ImportEntry::Named { imported: ExportName::Ident(b), local, .. }
                if *b == crux::intern_utf8("b") && *local == crux::intern_utf8("c")
        ));

        // Namespace imports, alone and after a default binding.
        let m = mod_ok("import * as ns from 'm';");
        let ModuleItem::Import(imp) = &m.body[0] else {
            panic!("expected an import declaration");
        };
        assert!(matches!(
            &imp.entries[0],
            ImportEntry::Namespace { local, .. } if *local == crux::intern_utf8("ns")
        ));
        let m = mod_ok("import d, * as ns from 'm';");
        let ModuleItem::Import(imp) = &m.body[0] else {
            panic!("expected an import declaration");
        };
        assert_eq!(imp.entries.len(), 2);
        assert!(matches!(imp.entries[0], ImportEntry::Default { .. }));
        assert!(matches!(imp.entries[1], ImportEntry::Namespace { .. }));

        // String export names and trailing commas.
        let m = mod_ok("import { 'str name' as local } from 'm';");
        let ModuleItem::Import(imp) = &m.body[0] else {
            panic!("expected an import declaration");
        };
        assert!(matches!(
            &imp.entries[0],
            ImportEntry::Named { imported: ExportName::Str(s), local, .. }
                if s == &JsString::from_utf8("str name") && *local == crux::intern_utf8("local")
        ));
        mod_ok("import { a, b, } from 'm';");
        mod_ok("import d, { a, } from 'm';");

        // Reserved words are valid ModuleExportNames when aliased.
        mod_ok("import { default as x } from 'm';");
        mod_ok("import { if as y } from 'm';");
        // `as` itself is a legal plain binding.
        mod_ok("import { as } from 'm';");

        // Side-effect-only imports and import attributes.
        let m = mod_ok("import 'side-effect';");
        let ModuleItem::Import(imp) = &m.body[0] else {
            panic!("expected an import declaration");
        };
        assert!(imp.entries.is_empty());
        assert_eq!(imp.specifier, JsString::from_utf8("side-effect"));
        let m = mod_ok("import d from './data.json' with { type: 'json' };");
        let ModuleItem::Import(imp) = &m.body[0] else {
            panic!("expected an import declaration");
        };
        assert_eq!(imp.attributes.len(), 1);
        assert!(matches!(
            imp.attributes[0].0,
            AttributeKey::Ident(k) if k == crux::intern_utf8("type")
        ));
        assert_eq!(imp.attributes[0].1, JsString::from_utf8("json"));
        mod_ok("import 'm' with { type: 'json' };");
    }

    #[test]
    fn parses_export_declarations() {
        // Named exports of local bindings.
        let m = mod_ok("export { a, b as c };");
        let ModuleItem::Export(ExportDecl::Named { specifiers, .. }) = &m.body[0] else {
            panic!("expected an export declaration");
        };
        assert_eq!(specifiers.len(), 2);
        assert!(matches!(
            &specifiers[0],
            ExportSpecifier::Same(ExportName::Ident(a)) if *a == crux::intern_utf8("a")
        ));
        assert!(matches!(
            &specifiers[1],
            ExportSpecifier::Alias { local: ExportName::Ident(b), exported: ExportName::Ident(c) }
                if *b == crux::intern_utf8("b") && *c == crux::intern_utf8("c")
        ));

        // Re-exports: `export … from`, star, and star-namespace.
        let m = mod_ok("export { a, b as c } from 'm';");
        assert!(matches!(
            m.body[0],
            ModuleItem::Export(ExportDecl::From { .. })
        ));
        let m = mod_ok("export * from 'm';");
        let ModuleItem::Export(ExportDecl::From { namespace, .. }) = &m.body[0] else {
            panic!("expected an export declaration");
        };
        assert!(namespace.is_none());
        let m = mod_ok("export * as ns from 'm';");
        let ModuleItem::Export(ExportDecl::From { namespace, .. }) = &m.body[0] else {
            panic!("expected an export declaration");
        };
        assert!(matches!(
            namespace,
            Some(ExportName::Ident(ns)) if *ns == crux::intern_utf8("ns")
        ));
        mod_ok("export { 'str' as 'out' } from 'm';");
        mod_ok("export { default } from 'm';");

        // Declaration exports.
        mod_ok("export var x;");
        mod_ok("export let x;");
        mod_ok("export const x = 1;");
        mod_ok("export function f() {}");
        mod_ok("export async function f() {}");
        mod_ok("export function* g() {}");
        mod_ok("export class A {}");
        let m = mod_ok("export const x = 1;");
        assert!(matches!(
            m.body[0],
            ModuleItem::Export(ExportDecl::Declaration(_))
        ));

        // Default exports: expressions, functions, and classes.
        let m = mod_ok("export default 1 + 2;");
        assert!(matches!(
            m.body[0],
            ModuleItem::Export(ExportDecl::Default(_))
        ));
        mod_ok("export default 'str';");
        mod_ok("export default function () {}");
        mod_ok("export default function f() {}");
        mod_ok("export default async function () {}");
        mod_ok("export default function* () {}");
        mod_ok("export default class {}");
        mod_ok("export default class A {}");
        // A named hoistable declaration after `default` keeps its name binding.
        let m = mod_ok("export default function f() {}");
        assert!(matches!(
            m.body[0],
            ModuleItem::Export(ExportDecl::Default(_))
        ));
    }

    #[test]
    fn module_expression_forms() {
        // import() and import.meta are expressions, not declarations.
        mod_ok("import('m');");
        mod_ok("import.meta;");
        mod_ok("import('m', { with: { type: 'json' } });");
        let m = mod_ok("const p = import('m');");
        assert!(matches!(m.body[0], ModuleItem::Stmt(_)));

        // Top-level await is legal in modules.
        mod_ok("await 1;");
        mod_ok("const p = await import('m');");
        mod_ok("export const p = await fetch('x');");
        // Ordinary statements still work at module top level.
        let m = mod_ok("let x = 1;");
        assert!(matches!(m.body[0], ModuleItem::Stmt(_)));
    }

    #[test]
    fn module_early_errors() {
        // Only one default export per module.
        mod_err("export default 1; export default 2;");
        mod_err("export default function () {} export default class {}");

        // await is a reserved word in module code.
        mod_err("import await from 'm';");
        mod_err("var await = 1;");
        mod_err("let await;");
        mod_err("export function await() {}");
        mod_err("function f() { var await; }");

        // Imports and exports only appear at module top level.
        mod_err("function f() { import x from 'm'; }");
        mod_err("{ import x from 'm'; }");
        mod_err("if (x) { export { y }; }");

        // Malformed import/export shapes.
        mod_err("import x;");
        mod_err("import x from;");
        mod_err("import x, y from 'm';");
        mod_err("import { x as } from 'm';");
        mod_err("export;");
        mod_err("export { x as } from 'm';");
        mod_err("export * from;");

        // Duplicate import bindings clash with lexical declarations.
        mod_err("import { a } from 'm'; let a;");
        mod_err("import a from 'm'; import a from 'n';");

        // HTML comments are rejected in modules (Annex B is script-only).
        mod_err("<!-- x");
        mod_err("--> x");

        // using declarations cannot be exported.
        mod_err("export using x = 1;");
        mod_err("export await using x = 1;");
        // ...but are fine inside exported function bodies.
        mod_ok("export function f() { using x = 1; }");

        // Module span covers the whole source.
        let m = mod_ok("import d from 'm';");
        assert_eq!(m.span, crux::Span::new(0, 18));
    }

    #[test]
    fn new_target_contexts() {
        // new.target is an early error outside function bodies (spec 13.3.4,
        // 15.2.2) — including inside arrows and computed class names.
        err("new.target;");
        err("() => new.target;");
        err("class A { [new.target]() {} }");
        mod_err("new.target;");
        // ...but legal inside functions, arrows, methods, field
        // initializers, and static blocks.
        ok("function f() { return new.target; }");
        ok("function f() { return () => new.target; }");
        ok("function f() { class A { [new.target]() {} } }");
        ok("class A { m() { return new.target; } }");
        ok("class A { x = new.target; }");
        ok("function f() { class A { x = new.target; } }");
        ok("class A { static { new.target; } }");
        ok("class A { static m() { return new.target; } }");
    }

    #[test]
    fn early_error_pass_labels() {
        // ContainsDuplicateLabels: labels thread through blocks.
        err("a: a: 1;");
        err("a: { a: 1; }");
        err("a: b: { a: 1; }");
        ok("a: b: 1;");
        ok("a: { b: 1; }");
        // Function bodies reset the label scope.
        ok("a: function f() { a: 1; }");
        err("a: function f() { continue a; }");
        err("a: for (;;) { function f() { continue a; } }");
        // ContainsUndefinedBreakTarget / ContainsUndefinedContinueTarget.
        ok("a: { break a; }");
        ok("a: b: for (;;) { continue a; break b; }");
        ok("a: while (1) { continue a; }");
        ok("a: switch (x) { case 1: break a; }");
        err("a: switch (x) { case 1: continue a; }");
        err("a: { continue a; }");
        err("break;");
        err("continue;");
        err("for (;;) { continue missing; }");
        ok("for (;;) { break; }");
        ok("for (;;) { continue; }");
        ok("switch (x) { case 1: break; }");
        // continue targets the nearest enclosing label chain; a label
        // threading through other labels to an iteration counts, but a
        // block boundary drops it (spec sec-syntax-directed-operations-
        // labels: labels fold into iterationSet only via BreakableStatement).
        ok("a: for (;;) { for (;;) { continue a; } }");
        ok("a: for (;;) { b: { continue a; } }");
        err("a: { b: for (;;) { continue a; } }");
        err("a: b: { for (;;) { continue a; } }");
        err("a: for (;;) { b: { continue b; } }");
        ok("a: b: for (;;) { continue a; continue b; }");
    }

    #[test]
    fn early_error_pass_module_exports() {
        // ReferencedBindings of `export { … }` must be plain identifiers.
        mod_err("export { default };");
        mod_err("export { \"str\" };");
        mod_err("export { if };");
        mod_ok("export { x as default };");
        mod_ok("export { default } from 'm';");
        mod_ok("export { \"str\" as x } from 'm';");
        // ExportedNames must be unique across the module.
        mod_err("export { a }; export { a };");
        mod_err("export { a } from 'm'; export { a } from 'n';");
        mod_err("export { a as default }; export default 1;");
        mod_err("export default 1; export default 2;");
        mod_err("export const x = 1; export { x };");
        mod_ok("export * from 'm'; export * from 'n';");
        mod_ok("export * as ns from 'm'; export * as ns2 from 'n';");
        mod_err("export * as ns from 'm'; export { ns };");
        mod_ok("export { a as b } from 'm'; export { a as c } from 'm';");
        mod_ok("export { a }; export { a as b };");
    }

    #[test]
    fn early_error_pass_class_bodies() {
        // `arguments` is an early error in field initializers and static
        // blocks; arrows inherit `arguments` (they have none of their own),
        // while nested functions have their own (spec 15.7.9 ContainsArguments).
        err("class A { x = arguments; }");
        err("class A { x = arguments[0]; }");
        err("class A { x = () => arguments; }");
        ok("class A { x = function () { return arguments; }; }");
        err("class A { static { arguments; } }");
        err("class A { static { () => arguments; } }");
        // `await` is an early error in static blocks; arrow bodies are
        // opaque to the check, function bodies are not.
        err("class A { static { await 1; } }");
        err("class A { static { await; } }");
        ok("class A { static { (async () => await 1); } }");
        ok("class A { static { async function f() { await 1; } } }");
    }

    #[test]
    fn early_error_pass_duplicate_proto() {
        err("({ __proto__: 1, __proto__: 2 });");
        err("({ __proto__: 1, \"__proto__\": 2 });");
        ok("({ __proto__: 1 });");
        // Shorthand and methods are not data properties.
        ok("({ __proto__, __proto__: 1 });");
        ok("({ __proto__: 1, __proto__() {} });");
        ok("({ __proto__: 1, [\"__proto__\"]: 2 });");
        // Computed names do not count.
        ok("({ [\"__proto__\"]: 1, [\"__proto__\"]: 2 });");
    }

    #[test]
    fn strict_legacy_octal_literals() {
        // Legacy octal and non-octal decimal integers are strict-mode errors.
        err("'use strict'; 0777;");
        err("'use strict'; 089;");
        err("'use strict'; 00;");
        err("function f() { 'use strict'; 0777; }");
        err("class A { m() { 0777; } }");
        ok("0777;");
        ok("function f() { 0777; }");
        // Other zero-prefixed forms are unaffected.
        ok("'use strict'; 0x1F;");
        ok("'use strict'; 0b101;");
        ok("'use strict'; 0o17;");
        ok("'use strict'; 0.5;");
    }

    #[test]
    fn string_escapes_strict_via_later_directive() {
        // A legacy octal/non-octal escape before a `"use strict"` directive
        // in the same prologue is an error (spec 12.9.4): the function is
        // strict and the escape is not part of strict EscapeSequence.
        err("function f() { \"\\1\"; \"use strict\"; }");
        err("function f() { \"\\8\"; \"use strict\"; }");
        err("function f() { \"\\052\"; \"use strict\"; }");
        err("(function() { \"asterisk: \\052\"; \"use strict\"; });");
        err("function f() { \"use strict\"; \"\\1\"; }");
        // Without a strict directive the same strings are sloppy-legal.
        ok("function f() { \"\\1\"; }");
        ok("function f() { \"\\8\"; }");
        // An escaped non-strict directive does not stop the prologue.
        ok("function f() { \"\\x41\"; \"use strict\"; }");
    }

    #[test]
    fn delete_unwraps_parentheses() {
        // The delete early errors see through parentheses (spec 13.6.2).
        err("'use strict'; delete (identifier);");
        err("'use strict'; delete ((identifier));");
        err("'use strict'; delete (((identifier)));");
        ok("delete (identifier);");
        ok("delete (a.b);");
    }

    #[test]
    fn new_target_context() {
        // `new.target` needs a real (non-arrow) enclosing function or a
        // class field initializer/static block (spec 13.3.4).
        err("new.target;");
        err("() => { new.target; };");
        err("function f() { new.t\\u0061rget; }");
        ok("function f() { new.target; }");
        ok("function g() { return () => new.target; }");
        ok("() => { function f() { new.target; } };");
        ok("class C { static { new.target; } }");
        ok("class C { x = new.target; }");
        ok("new (function() { return new.target; });");
    }

    #[test]
    fn do_while_semicolon_asi() {
        // The do-while terminating semicolon is inserted before the next
        // token when the previous token is `)` (spec 12.10.1).
        ok("do {} while (0) x = 42;");
        ok("do do do ; while (x) while (x) while (x) x = 39;");
        ok("do break ; while (0) x = 42;");
        ok("do {} while (0);");
        ok("do {} while (0)");
    }

    #[test]
    fn static_block_accessor_await_is_an_identifier() {
        // An accessor method's params and body reset the [Await] context: a
        // static block's reserved `await` does not leak in (spec 15.7.13).
        ok("class C { static { ({ set accessor(await) {} }); } }");
        ok(
            "var await = 0; class C { static { ({ set accessor(x = await) { await; } }).accessor = undefined; } }",
        );
        err("class C { static { ({ async m(await) {} }); } }");
    }

    #[test]
    fn let_as_lexically_bound_name() {
        // `let` is never a valid lexically bound name (spec 14.2.1).
        err("let let = 1;");
        err("const let = 1;");
        err("let [let] = a;");
        err("using let = 1;");
        err("for (let let of x) {}");
        err("for (const let of x) {}");
        err("for (using let of x) {}");
        // `var let` and class/function names are unrestricted in sloppy code.
        ok("var let = 1;");
        ok("for (var let of x) {}");
        // A class definition is always strict mode code (spec 15.7.3), so
        // `let` is not a valid class name; function names are unrestricted.
        err("class let {}");
        ok("function let() {}");
        // Strict mode rejects `let` as any identifier.
        err("'use strict'; var let = 1;");
        err("'use strict'; let;");
    }

    #[test]
    fn parses_annex_b_forin_initializer() {
        let s = stmt("for (var x = 0 in obj) {}");
        assert!(matches!(
            s.kind,
            StmtKind::ForIn {
                left: ForBinding::VarDecl {
                    kind: VarDeclKind::Var,
                    pattern: syntax::BindingPattern::Ident(_),
                    init: Some(_),
                },
                ..
            }
        ));
        // The initializer is parsed with `[~In]`.
        ok("for (var x = a in obj) {}");
        // Patterns are not Annex B forms; they stay errors.
        err("for (var [a] = b in obj) {}");
    }

    #[test]
    fn rest_element_must_be_last_in_assignment_targets() {
        // Assignment patterns, arrow parameters, and for-of heads (spec
        // 13.15.1/13.2.2 early errors).
        err("[...a, b] = c");
        err("[a, ...b, c] = d");
        err("({...a, b} = o)");
        err("({...a, b = 1} = o)");
        err("([...a, b]) => x");
        err("for ([...a, b] of x) {}");
        ok("[a, ...b] = c");
        ok("({...a} = o)");
    }

    #[test]
    fn catch_parameter_redeclaration_rules() {
        // spec 15.1.8: CatchParameter names clash with the block's
        // LexicallyDeclaredNames (let/class/function), but `var` in the
        // catch body may share the name (Annex B).
        ok("try {} catch (e) { var e; }");
        ok("var e; try {} catch (e) {}");
        ok("try {} catch (e) { var e; } var e;");
        err("try {} catch (e) { function e() {} }");
        err("try {} catch (e) { let e; }");
        err("try {} catch (e) { const e = 1; }");
        // A nested block shadows the catch parameter instead of clashing.
        ok("try {} catch (e) { { let e; } }");
        err("try {} catch ([x, x]) {}");
    }

    #[test]
    fn parses_using_declarations() {
        // Statement forms, with required initializers. A `using` declaration
        // must be contained in a block/function body, never at script top
        // level (spec 14.5.1).
        err("using x = 1;");
        let s = stmt("{ using x = 1; }");
        let StmtKind::Block(block) = s.kind else {
            panic!("expected a block");
        };
        assert!(matches!(
            block.stmts[0].kind,
            StmtKind::UsingDecl {
                is_await: false,
                ref decls,
            } if decls.len() == 1
        ));
        let s = stmt("{ using x = 1, y = 2; }");
        let StmtKind::Block(block) = s.kind else {
            panic!("expected a block");
        };
        let StmtKind::UsingDecl {
            is_await: false,
            ref decls,
        } = block.stmts[0].kind
        else {
            panic!("expected a using declaration");
        };
        assert_eq!(decls.len(), 2);
        assert!(matches!(decls[0].pattern, syntax::BindingPattern::Ident(_)));
        assert!(decls[0].init.is_some());

        // await using in async functions and modules.
        ok("async function f() { await using x = 1; }");
        mod_ok("using x = 1;");
        mod_ok("await using x = 1;");
        mod_ok("async function f() { await using x = 1, y = 2; }");
        ok("{ using x = 1; }");

        // `using` stays an ordinary identifier everywhere else.
        ok("using;");
        ok("using = 5;");
        ok("using();");
        ok("var using = 5;");
        ok("let using;");
        ok("using.x;");
        ok("using [0] = 1;");
        ok("using in obj;");
        ok("f(using);");
        ok("let x = using;");

        // for-of heads: `using` and `await using` bindings.
        assert!(matches!(
            stmt("for (using x of arr) {}").kind,
            StmtKind::ForOf {
                left: ForBinding::VarDecl {
                    kind: VarDeclKind::Using,
                    ..
                },
                ..
            }
        ));
        ok("async function f() { for (await using x of arr) {} }");
        ok("async function f() { for await (using x of arr) {} }");
        ok("async function f() { for await (await using x of arr) {} }");
        // `async` is a plain identifier head, not an async arrow, when no
        // `=>` follows (spec ForInOfStatement: `for await (async of …)`).
        ok("let async; async function f() { for await (async of [7]); }");
        // An async arrow still parses.
        ok("async function f() { var g = async x => x; }");
        // `for (async of …)` is a SyntaxError: the expression-headed for-of
        // production has the lookahead `[lookahead ∉ { let, async of }]`
        // (spec 14.7.5), so `async` cannot be the LHS. An escaped `async` is
        // an ordinary identifier, and `async of => …` is an arrow init, so
        // both stay valid.
        err("let async; for (async of [1]);");
        ok(r"let async; for (\u0061sync of [1]);");
        ok("for (async of => {}; false; ) {}");
        // `for (using of y)` is an expression-headed for-of.
        assert!(matches!(
            stmt("for (using of arr) {}").kind,
            StmtKind::ForOf {
                left: ForBinding::Expr(_),
                ..
            }
        ));
        // `using` in a classic for head.
        assert!(matches!(
            stmt("for (using x = 0; x < 3; x++) {}").kind,
            StmtKind::For {
                init: Some(ForInit::VarDecl {
                    kind: VarDeclKind::Using,
                    ..
                }),
                ..
            }
        ));
        ok("for (using [a] = b; ;) {}");
    }

    #[test]
    fn using_early_errors() {
        // Initializers are required (using bindings are constant).
        err("using x;");
        err("using x, y = 1;");
        err("async function f() { await using x; }");
        err("for (using x; ;) {}");
        // Initializers are forbidden in for-in/of heads.
        err("for (using x = 1 of arr) {}");
        // No destructuring in statement-level using declarations (~Pattern).
        err("using { a } = b;");
        // `using` has no for-in form.
        err("for (using x in obj) {}");
        err("async function f() { for (await using x in obj) {} }");
        // for await needs an async context and `of`.
        err("for await (using x of arr) {}");
        err("for await (x in obj) {}");
        // Duplicate and conflicting bindings.
        err("using x = 1; using x = 2;");
        err("using x = 1; let x;");
        err("let x; using x = 1;");
        // await using requires an await-legal context.
        err("await using x = 1;");
        err("function f() { await using x = 1; }");
        // A line terminator after `using` triggers ASI, not a declaration.
        ok("using\nx = 1;");
        // Keyword bindings are rejected.
        err("using in = 1;");
        err("using let = 1;");
    }

    #[test]
    fn parse_function_parses_function_expressions() {
        let f = parse_function("function anonymous(a, b\n) {\nreturn a + b\n}").unwrap();
        assert_eq!(f.name, Some(crux::intern_utf8("anonymous")));
        assert_eq!(f.params.len(), 2);
        // The expression must be fully consumed.
        assert!(parse_function("function f() {} extra").is_err());
        // Invalid bodies and parameter lists are syntax errors.
        assert!(parse_function("function f(a b) {}").is_err());
        assert!(parse_function("function f() { {").is_err());
    }

    #[test]
    fn asi_statement_matrix() {
        // ASI separates two lexical declarations and two assignments.
        let program = ok("let a = 1\nlet b = 2");
        assert_eq!(program.body.len(), 2);
        assert!(program.body.iter().all(|s| matches!(
            s.kind,
            StmtKind::VarDecl {
                kind: VarDeclKind::Let,
                ..
            }
        )));
        let program = ok("a = 1\nb = 2");
        assert_eq!(program.body.len(), 2);
        assert!(
            program
                .body
                .iter()
                .all(|s| matches!(s.kind, StmtKind::Expr(_)))
        );

        // `return` never takes an argument across a line terminator.
        let program = ok("function f() { return\nx; }");
        let StmtKind::FunctionDecl(f) = &program.body[0].kind else {
            panic!("expected function declaration")
        };
        assert!(matches!(f.body.stmts[0].kind, StmtKind::Return(None)));
        assert!(matches!(f.body.stmts[1].kind, StmtKind::Expr(_)));

        // `break` cannot take a label across a line terminator: the label
        // line becomes its own expression statement.
        let program = ok("label: for (;;) { break\nlabel; }");
        let StmtKind::Labeled { body, .. } = &program.body[0].kind else {
            panic!("expected labeled statement")
        };
        let StmtKind::For {
            body: loop_body, ..
        } = &body.kind
        else {
            panic!("expected for statement")
        };
        let StmtKind::Block(block) = &loop_body.kind else {
            panic!("expected block")
        };
        assert_eq!(block.stmts.len(), 2);
        assert!(matches!(block.stmts[0].kind, StmtKind::Break(None)));
        assert!(matches!(block.stmts[1].kind, StmtKind::Expr(_)));

        // Restricted postfix `++`: `a` and `++b` are separate statements.
        let program = ok("a\n++b");
        assert_eq!(program.body.len(), 2);
        assert!(matches!(
            program.body[1].kind,
            StmtKind::Expr(Expr {
                kind: ExprKind::Update { prefix: true, .. },
                ..
            })
        ));

        // A `(` on the next line continues the call: `x = y(a + b)`.
        let expr = expr_stmt("x = y\n(a + b)");
        let ExprKind::Assign { op, value, .. } = expr.kind else {
            panic!("expected assignment")
        };
        assert_eq!(op, syntax::AssignOp::Assign);
        assert!(matches!(
            value.kind,
            ExprKind::Call(c) if matches!(c.callee.kind, ExprKind::Ident(_))
        ));

        // `else` always binds to the nearest preceding `if` (no ASI before
        // else).
        let s = stmt("if (a) b\nelse c");
        assert!(matches!(
            s.kind,
            StmtKind::If {
                alternate: Some(_),
                ..
            }
        ));

        // A do-while ends at its `)`: the next line is a fresh statement.
        let program = ok("do {} while (a)\nb");
        assert_eq!(program.body.len(), 2);
        assert!(matches!(program.body[0].kind, StmtKind::DoWhile { .. }));
        assert!(matches!(program.body[1].kind, StmtKind::Expr(_)));
    }

    #[test]
    fn destructuring_assignment_edge_cases() {
        // `({a = f()} = {})` — a shorthand default inside an assignment
        // pattern target.
        let expr = expr_stmt("({a = f()} = {})");
        let ExprKind::Paren(inner) = expr.kind else {
            panic!("expected parenthesized assignment")
        };
        let ExprKind::Assign { op, target, .. } = inner.kind else {
            panic!("expected assignment")
        };
        assert_eq!(op, syntax::AssignOp::Assign);
        assert!(matches!(target.kind, ExprKind::Object(_)));

        // Array and object assignment patterns: defaults, nesting, rest.
        ok("[a, b = c] = d");
        ok("({x: {y} = {}} = o)");
        ok("[a, ...rest] = arr");
        ok("({a, ...rest} = o)");

        // The same cover form is an arrow parameter or an object-literal
        // expression depending on what follows.
        assert!(matches!(
            expr_stmt("({a}) => a").kind,
            ExprKind::Arrow { .. }
        ));
        assert!(matches!(
            expr_stmt("({a})").kind,
            ExprKind::Paren(inner) if matches!(inner.kind, ExprKind::Object(_))
        ));
    }

    #[test]
    fn exponentiation_precedence_and_restrictions() {
        // `**` is right-associative: 2 ** (3 ** 2).
        let (op, _, right) = binary("2 ** 3 ** 2");
        assert_eq!(op, BinaryOp::Exp);
        assert!(matches!(
            right.kind,
            ExprKind::Binary {
                op: BinaryOp::Exp,
                ..
            }
        ));

        // A unary expression on the left of `**` is an early error, but the
        // same unary on the right (or parenthesized) is fine.
        err("-2 ** 2");
        ok("(-2) ** 2");
        let (op, _, right) = binary("2 ** -2");
        assert_eq!(op, BinaryOp::Exp);
        assert!(matches!(
            right.kind,
            ExprKind::Unary {
                op: UnaryOp::Minus,
                ..
            }
        ));

        // `**` binds tighter than `*`: 2 ** 3 * 4 = (2 ** 3) * 4.
        let (op, left, _) = binary("2 ** 3 * 4");
        assert_eq!(op, BinaryOp::Mul);
        assert!(matches!(
            left.kind,
            ExprKind::Binary {
                op: BinaryOp::Exp,
                ..
            }
        ));
    }

    #[test]
    fn cover_grammar_arrow_disambiguation() {
        // Parenthesized, defaulted, and rest arrow parameters.
        ok("(a, b) => a + b");
        ok("(a = 1) => a");
        ok("(a, ...b) => a");
        ok("(a, b, ...c) => d");
        assert!(matches!(
            expr_stmt("async (a) => a").kind,
            ExprKind::Arrow { is_async: true, .. }
        ));

        // Without `=>` the same parentheses are a sequence expression...
        let expr = expr_stmt("(a, b)");
        assert!(matches!(
            expr.kind,
            ExprKind::Paren(inner) if matches!(inner.kind, ExprKind::Sequence(_))
        ));
        // ...but a spread list has no expression reading.
        err("(a, b, ...c)");

        // `({a: 1})` is an object literal; `{a: 1}` is a labeled block.
        assert!(matches!(
            expr_stmt("({a: 1})").kind,
            ExprKind::Paren(inner) if matches!(inner.kind, ExprKind::Object(_))
        ));
        assert!(matches!(stmt("{a: 1}").kind, StmtKind::Block(_)));

        // Literal covers cannot become arrow parameters.
        err("(0, 0) => 0");
    }

    #[test]
    fn early_error_syntax_matrix() {
        // Duplicate parameters: sloppy simple lists only.
        ok("function f(a, a) {}");
        err("function f(a, a = 1) {}");

        // Duplicate and conflicting lexical declarations; missing const init.
        err("let a; let a;");
        err("var a; let a;");
        err("const a;");

        // `let` and `yield` as binding names in strict/generator contexts.
        err("'use strict'; function f(let) {}");
        err("'use strict'; var yield = 1;");
        err("function* g() { var yield; }");
        ok("function f(yield) {}");
        ok("var yield = 1;");

        // Strict-mode `with`, bare `super`, return outside a function.
        err("'use strict'; with (x) {}");
        err("super;");
        err("return;");

        // break/continue must resolve to a label or an enclosing loop.
        err("a: for (;;) { break b; }");
        err("a: for (;;) { continue b; }");

        // await is a plain identifier in scripts: `await 1` cannot parse.
        ok("var await = 1;");
        ok("await;");
        err("await 1;");
    }

    #[test]
    fn for_head_forms() {
        // Classic for head with a lexical binding.
        let s = stmt("for (let i = 0; i < 3; i++) {}");
        assert!(matches!(
            s.kind,
            StmtKind::For {
                init: Some(ForInit::VarDecl {
                    kind: VarDeclKind::Let,
                    ..
                }),
                ..
            }
        ));

        // for-of heads: lexical, var, and expression bindings.
        let s = stmt("for (const x of [1, 2]) {}");
        assert!(matches!(
            s.kind,
            StmtKind::ForOf {
                left: ForBinding::VarDecl {
                    kind: VarDeclKind::Const,
                    ..
                },
                ..
            }
        ));
        let s = stmt("for (var x of [1, 2]) {}");
        assert!(matches!(
            s.kind,
            StmtKind::ForOf {
                left: ForBinding::VarDecl {
                    kind: VarDeclKind::Var,
                    ..
                },
                ..
            }
        ));
        let s = stmt("for (x of [1]) {}");
        assert!(matches!(
            s.kind,
            StmtKind::ForOf {
                left: ForBinding::Expr(_),
                ..
            }
        ));

        // for-in head with a var binding.
        let s = stmt("for (var x in { a: 1 }) {}");
        assert!(matches!(
            s.kind,
            StmtKind::ForIn {
                left: ForBinding::VarDecl {
                    kind: VarDeclKind::Var,
                    ..
                },
                ..
            }
        ));

        // Destructuring in expression-headed for-of.
        ok("for ([a, b] of pairs) {}");
        ok("for ({ x } of objs) {}");

        // Annex B initializer is identifier-only var in sloppy code.
        ok("for (var x = 1 in obj) {}");
        err("for (var { x } = y in obj) {}");
    }

    #[test]
    fn private_identifiers_require_an_enclosing_class() {
        // AllPrivateIdentifiersValid: a PrivateIdentifier outside any class
        // is a SyntaxError (spec 13.4/13.11), so dynamic function bodies
        // like `new Function('o.#f')` fail at CreateDynamicFunction.
        err("o.#f;");
        err("#f in o;");
        err("function g() { return this.#f; }");
        // Inside a class the name may be declared and is accepted.
        ok("class C { #f; m() { return this.#f; } }");
        ok("class C { #f; m(o) { return #f in o; } }");
    }

    #[test]
    fn for_loop_heads_are_loop_scoped() {
        // A classic lexical head does not declare into the enclosing list: a
        // sibling loop, a later `let`, or an enclosing `var` may reuse the name.
        ok("for (let i = 0; i < 3; i++) {} for (let i = 0; i < 3; i++) {}");
        ok("for (let i = 0; i < 3; i++) {} let i = 1;");
        ok("let i = 1; for (let i = 0; i < 3; i++) {}");
        ok("for (let i = 0; i < 3; i++) {} var i;");
        // The head still clashes with `var` names in its own body (14.7.4).
        err("for (let i = 0; i < 3; i++) { var i; }");
        err("for (const i = 0; i < 3; i++) { var i; }");
    }
}
