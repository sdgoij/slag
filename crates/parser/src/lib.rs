//! Recursive-descent parser: syntactic grammar, cover grammar, early errors,
//! and ASI (spec ch. 13-17).
//!
//! Phase 3 implements the parameterized grammar `[Yield, Await, Return, In]`
//! and the syntax-directed operations the evaluator needs.

mod class;
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

/// Parses a Module (spec 16.2): import/export declarations plus a strict
/// statement list. Modules are always strict, `await` is reserved, top-level
/// `await` is allowed, and HTML comments are rejected.
pub fn parse_module(source: &str) -> Result<Module, JsError> {
    let source = SourceText::from_utf8(source);
    let mut parser = Parser::new(&source, false);
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
    Ok(Module { body, span })
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
        // Duplicate function declarations are strict-mode errors.
        ok("function f() {} function f() {}");
        err("'use strict'; function f() {} function f() {}");
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
    fn parses_annex_b_forin_initializer() {
        // Annex B.2.6: `for (var x = init in obj)` in sloppy code.
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
    fn parses_using_declarations() {
        // Statement forms, with required initializers.
        assert!(matches!(
            stmt("using x = 1;").kind,
            StmtKind::UsingDecl {
                is_await: false,
                ref decls,
            } if decls.len() == 1
        ));
        let s = stmt("using x = 1, y = 2;");
        let StmtKind::UsingDecl {
            is_await: false,
            ref decls,
        } = s.kind
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
}
