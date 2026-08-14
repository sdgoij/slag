//! AST node types for the syntactic grammar (spec ch. 13-17).
//!
//! Every node carries a `Span` into the original source text: stack traces
//! and `Function.prototype.toString` need exact original ranges. The
//! parameterized grammar `[Yield, Await, Return, In]` is resolved at parse
//! time; the tree below is parameter-free.

use crux::{AtomId, BigInt, JsString, Span};

/// A Script or Module: the body of statements plus the whole source span.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// A Module (spec 16.2): a list of statements, import declarations, and
/// export declarations.
#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub body: Vec<ModuleItem>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModuleItem {
    Stmt(Stmt),
    Import(ImportDecl),
    Export(ExportDecl),
}

/// An `import` declaration (spec 16.2.2).
#[derive(Debug, Clone, PartialEq)]
pub struct ImportDecl {
    pub span: Span,
    pub specifier: JsString,
    pub entries: Vec<ImportEntry>,
    pub attributes: Vec<(AttributeKey, JsString)>,
}

/// One binding of an import declaration.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportEntry {
    /// `import default from …`.
    Default { local: AtomId, span: Span },
    /// `import * as ns from …`.
    Namespace { local: AtomId, span: Span },
    /// `import { x as local } from …`.
    Named {
        imported: ExportName,
        local: AtomId,
        span: Span,
    },
}

/// A module export name: an identifier name or a string literal.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportName {
    Ident(AtomId),
    Str(JsString),
}

/// An `export` declaration (spec 16.2.3).
#[derive(Debug, Clone, PartialEq)]
pub enum ExportDecl {
    /// `export { a, b as c };` — local names.
    Named {
        specifiers: Vec<ExportSpecifier>,
        span: Span,
    },
    /// `export … from "mod";` — re-exports (incl. `export *`).
    From {
        specifiers: Vec<ExportSpecifier>,
        /// `export * as ns from …`.
        namespace: Option<ExportName>,
        specifier: JsString,
        attributes: Vec<(AttributeKey, JsString)>,
        span: Span,
    },
    /// `export var …;` / `export function …` / `export class …`.
    Declaration(Stmt),
    /// `export default …`.
    Default(Box<ExportDefault>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExportSpecifier {
    /// `name` — the local and exported names coincide.
    Same(ExportName),
    /// `local as exported`.
    Alias {
        local: ExportName,
        exported: ExportName,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExportDefault {
    Function(Function),
    Class(Class),
    Expr(Expr),
}

/// An import-attribute key (import attributes / `with { … }`).
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeKey {
    Ident(AtomId),
    Str(JsString),
}

/// A statement node: `span` covers the entire statement (spec ch. 14).
#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub span: Span,
    pub kind: StmtKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    /// `{ StatementList_opt }`
    Block(Block),
    /// `;`
    Empty,
    /// `Expression ;`
    Expr(Expr),
    /// `if ( Expression ) Statement else Statement_opt`
    If {
        test: Expr,
        consequent: Box<Stmt>,
        alternate: Option<Box<Stmt>>,
    },
    /// `var`/`let`/`const` declaration (spec 14.3.2, 14.2).
    VarDecl {
        kind: VarDeclKind,
        decls: Vec<VarDeclarator>,
    },
    /// `using`/`await using` declaration (spec 15.14): binds disposables
    /// that are disposed at scope exit. Bindings are identifier-only with
    /// required initializers.
    UsingDecl {
        is_await: bool,
        decls: Vec<VarDeclarator>,
    },
    /// `function` declaration (spec 15.2).
    FunctionDecl(Function),
    /// `class` declaration (spec 15.7) — the name is required.
    ClassDecl(Class),
    /// `return [Expression] ;` — the expression is restricted by
    /// `[no LineTerminator here]`.
    Return(Option<Expr>),
    /// `LabelIdentifier : Statement`
    Labeled { label: AtomId, body: Box<Stmt> },
    /// `break [LabelIdentifier] ;`
    Break(Option<AtomId>),
    /// `continue [LabelIdentifier] ;`
    Continue(Option<AtomId>),
    /// `while ( Expression ) Statement`
    While { test: Expr, body: Box<Stmt> },
    /// `do Statement while ( Expression ) ;`
    DoWhile { body: Box<Stmt>, test: Expr },
    /// `for ( Init_opt ; Test_opt ; Update_opt ) Statement`
    For {
        init: Option<ForInit>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: Box<Stmt>,
    },
    /// `for ( LeftHandSideExpression in Expression ) Statement`
    ForIn {
        left: ForBinding,
        right: Expr,
        body: Box<Stmt>,
    },
    /// `for [await] ( LeftHandSideExpression of Expression ) Statement`
    ForOf {
        left: ForBinding,
        right: Expr,
        body: Box<Stmt>,
        is_await: bool,
    },
    /// `throw Expression ;` — the expression is restricted by
    /// `[no LineTerminator here]`.
    Throw(Expr),
    /// `try Block Catch_opt Finally_opt`
    Try {
        block: Block,
        handler: Option<CatchClause>,
        finalizer: Option<Block>,
    },
    /// `switch ( Expression ) CaseBlock`
    Switch {
        discriminant: Expr,
        cases: Vec<SwitchCase>,
    },
    /// `debugger ;`
    Debugger,
    /// `with ( Expression ) Statement` — Annex B, strict-mode early error.
    With { object: Expr, body: Box<Stmt> },
}

/// Declaration kinds for `var` statements and `for` heads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarDeclKind {
    Var,
    Let,
    Const,
    /// `for (using x of …)` — resource binding, `of` only.
    Using,
    /// `for (await using x of …)` — async resource binding, `of` only.
    AwaitUsing,
}

/// One declarator of a variable declaration: `BindingPattern Initializer_opt`.
#[derive(Debug, Clone, PartialEq)]
pub struct VarDeclarator {
    pub pattern: BindingPattern,
    pub init: Option<Expr>,
    pub span: Span,
}

/// The initializer slot of a `for` statement (spec 14.7.4).
#[derive(Debug, Clone, PartialEq)]
pub enum ForInit {
    Expr(Expr),
    VarDecl {
        kind: VarDeclKind,
        decls: Vec<VarDeclarator>,
    },
}

/// The left-hand side of `for-in`/`for-of` (spec 14.7.5): a left-hand-side
/// expression (identifier, member access, or destructuring pattern) or a
/// single `var`/`let`/`const` declarator without an initializer.
#[derive(Debug, Clone, PartialEq)]
pub enum ForBinding {
    Expr(Expr),
    VarDecl {
        kind: VarDeclKind,
        pattern: BindingPattern,
        /// Annex B.2.6: `for (var x = init in obj)` — a var initializer in a
        /// for-in head, evaluated per iteration (sloppy code only).
        init: Option<Expr>,
    },
}

/// `catch ( CatchParameter_opt ) Block` — the parameter is optional since
/// ES2019.
#[derive(Debug, Clone, PartialEq)]
pub struct CatchClause {
    pub param: Option<BindingPattern>,
    pub body: Block,
    pub span: Span,
}

/// One `case`/`default` clause of a switch (spec 14.12).
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchCase {
    /// `None` for `default`.
    pub test: Option<Expr>,
    pub consequent: Vec<Stmt>,
    pub span: Span,
}

/// A statement block (spec 14.3).
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

/// An expression node: `span` covers the whole expression (spec ch. 13).
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    /// Null, boolean, numeric, string, and regexp literals (spec 13.2.4).
    Literal(Literal),
    /// `IdentifierReference`
    Ident(AtomId),
    /// `this`
    This,
    /// `super` — always followed by `.`/`(` in valid code.
    Super,
    /// `[ Elision_opt AssignmentElementList ]` (spec 13.2.4)
    Array(ArrayLiteral),
    /// `{ PropertyDefinitionList }` (spec 13.2.5)
    Object(ObjectLiteral),
    /// `function` expression (spec 15.2.3).
    Function(Function),
    /// `class` expression (spec 15.7.7) — the name is optional.
    Class(Box<Class>),
    /// Unary operators: `delete`, `void`, `typeof`, `+`, `-`, `~`, `!`.
    Unary { op: UnaryOp, operand: Box<Expr> },
    /// Prefix/postfix `++`/`--`.
    Update {
        op: UpdateOp,
        prefix: bool,
        target: Box<Expr>,
    },
    /// Binary operators (spec 13.6-13.12), excluding logical ones.
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `&&`, `||`, `??` (spec 13.13) — short-circuit semantics at eval time.
    Logical {
        op: LogicalOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `target = value` and compound assignments (spec 13.15).
    Assign {
        op: AssignOp,
        target: Box<Expr>,
        value: Box<Expr>,
    },
    /// `test ? consequent : alternate` (spec 13.14).
    Conditional {
        test: Box<Expr>,
        consequent: Box<Expr>,
        alternate: Box<Expr>,
    },
    /// `#name in object` — the private brand check (spec 13.11.1).
    PrivateIn { name: AtomId, object: Box<Expr> },
    /// `callee ( Arguments )`, incl. optional-call `callee?.(…)`.
    Call(CallExpr),
    /// `new callee ( Arguments )` and `new.target`.
    New(NewExpr),
    /// `.name`, `[expr]`, `.#private` accesses, incl. optional `?.`.
    Member(MemberExpr),
    /// `` tag`…` `` (spec 13.3.6).
    TaggedTemplate {
        tag: Box<Expr>,
        quasi: TemplateLiteral,
    },
    /// Untagged template literal.
    Template(TemplateLiteral),
    /// `( params ) => body` (spec 13.2.2).
    Arrow {
        is_async: bool,
        params: Vec<BindingElement>,
        body: ArrowBody,
    },
    /// `( expr )` — grouping; preserved because it affects `new` binding and
    /// `Function.prototype.toString` output.
    Paren(Box<Expr>),
    /// `expr1 , expr2` (spec 13.16).
    Sequence(Vec<Expr>),
    /// `yield`, `yield expr`, `yield* expr` (spec 13.3.2).
    Yield {
        delegate: bool,
        argument: Option<Box<Expr>>,
    },
    /// `await expr` (spec 13.2.3).
    Await(Box<Expr>),
    /// `new.target`, `import.meta`.
    MetaProperty { meta: AtomId, property: AtomId },
    /// `import ( specifier )` / `import ( specifier , options )`.
    ImportCall {
        specifier: Box<Expr>,
        options: Option<Box<Expr>>,
    },
}

/// Literal values (spec 13.2.4). Strings are cooked values; regexps carry the
/// raw pattern and flags text.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Boolean(bool),
    Number(f64),
    BigInt(BigInt),
    Str(JsString),
    RegExp { pattern: JsString, flags: JsString },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Delete,
    Void,
    Typeof,
    Plus,
    Minus,
    BitNot,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    Increment,
    Decrement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Exp,
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    LeftShift,
    RightShift,
    UnsignedRightShift,
    LessThan,
    GreaterThan,
    LessEqual,
    GreaterEqual,
    In,
    Instanceof,
    Equal,
    NotEqual,
    StrictEqual,
    StrictNotEqual,
    BitAnd,
    BitXor,
    BitOr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
    Nullish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    RemAssign,
    ExpAssign,
    LeftShiftAssign,
    RightShiftAssign,
    UnsignedRightShiftAssign,
    BitAndAssign,
    BitXorAssign,
    BitOrAssign,
    AndAssign,
    OrAssign,
    NullishAssign,
}

/// `[ elision ]` array literal; each element is a value, a spread, or a hole.
#[derive(Debug, Clone, PartialEq)]
pub struct ArrayLiteral {
    pub elements: Vec<ArrayElement>,
    /// A trailing comma directly after the rest element (`[...x,]`): an
    /// elision may not follow an AssignmentRestElement (spec 13.15.5.1). The
    /// AST keeps it separate from `Hole` because a trailing comma adds no
    /// element in expression position.
    pub rest_trailing_comma: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrayElement {
    /// An elision hole: `[a, , b]`.
    Hole,
    Expr(Expr),
    /// `...expr`.
    Spread(Expr),
}

/// `{ PropertyDefinitionList }`.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectLiteral {
    pub props: Vec<ObjectProperty>,
    /// A trailing comma directly after the rest property (`{...x,}`): an
    /// element may not follow an AssignmentRestProperty (spec 13.15.5.1).
    pub rest_trailing_comma: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ObjectProperty {
    /// `key : value` or `key` shorthand. `shorthand` distinguishes
    /// `{ x }` (an IdentifierReference) from `{ x: x }` for the duplicate
    /// `__proto__` early error.
    Init {
        key: PropertyName,
        value: Expr,
        shorthand: bool,
    },
    /// `key ( params ) { body }` incl. `async`/`*` methods.
    Method {
        key: PropertyName,
        function: Function,
    },
    /// `get key ( ) { body }`.
    Get { key: PropertyName, body: Block },
    /// `set key ( param ) { body }`. The parameter may carry a default
    /// initializer (`set x(v = 1) {}`).
    Set {
        key: PropertyName,
        param: BindingPattern,
        /// The setter parameter's initializer, if any.
        init: Option<Expr>,
        body: Block,
    },
    /// `...expr`.
    Spread(Expr),
}

/// Property names in object literals, patterns, and member expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyName {
    Ident(AtomId),
    Str(JsString),
    Number(f64),
    Computed(Expr),
}

/// One argument of a call or `new` expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Argument {
    Expr(Expr),
    /// `...expr`.
    Spread(Expr),
}

/// `callee ( Arguments )` with the optional-call marker.
#[derive(Debug, Clone, PartialEq)]
pub struct CallExpr {
    pub callee: Box<Expr>,
    pub args: Vec<Argument>,
    /// True when this link used `?.(…)`; later chain links short-circuit too.
    pub optional: bool,
    pub span: Span,
}

/// `new callee ( Arguments )`; args are empty when the parentheses are
/// omitted (`new Foo`).
#[derive(Debug, Clone, PartialEq)]
pub struct NewExpr {
    pub callee: Box<Expr>,
    pub args: Vec<Argument>,
    pub span: Span,
}

/// A member access: `object . name`, `object [ expr ]`, `object . #private`.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberExpr {
    pub object: Box<Expr>,
    pub property: MemberProperty,
    /// True for the link that used `?.`; later links in the chain are not
    /// evaluated when the chain short-circuits.
    pub optional: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MemberProperty {
    Name(AtomId),
    Private(AtomId),
    Computed(Box<Expr>),
}

/// A template literal: `` `a${expr}b${expr}c` `` becomes quasis `[a, b, c]`
/// and exprs `[expr, expr]`. `cooked` is `None` when the quasi contains a
/// NotEscapeSequence (only legal for tagged templates).
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateLiteral {
    pub quasis: Vec<TemplateElement>,
    pub exprs: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TemplateElement {
    pub cooked: Option<JsString>,
    pub raw: JsString,
    pub span: Span,
}

/// Arrow function body: a concise expression or a block.
#[derive(Debug, Clone, PartialEq)]
pub enum ArrowBody {
    Expr(Box<Expr>),
    Block(Block),
}

/// A function (declaration, expression, or method). Parameters are binding
/// elements so destructuring and defaults are first-class.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub span: Span,
    /// `None` for anonymous function expressions.
    pub name: Option<AtomId>,
    pub params: Vec<BindingElement>,
    pub body: Block,
    pub is_async: bool,
    pub is_generator: bool,
    /// Annex B: this FunctionDeclaration appeared in statement position
    /// (`if (x) function f(){}`) rather than as a StatementListItem.
    pub statement_position: bool,
}

/// A class declaration or expression (spec 15.7).
#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    pub span: Span,
    /// `None` for anonymous class expressions.
    pub name: Option<AtomId>,
    /// The `extends` heritage, if any.
    pub heritage: Option<Expr>,
    pub elements: Vec<ClassElement>,
}

/// The name of a class element: a property name or a private identifier.
#[derive(Debug, Clone, PartialEq)]
pub enum ClassElementName {
    Property(PropertyName),
    Private(AtomId),
}

/// One element of a class body (spec 15.7.5).
#[derive(Debug, Clone, PartialEq)]
pub enum ClassElement {
    /// A method: plain, `async`, generator, or `async`-generator.
    Method {
        is_static: bool,
        name: ClassElementName,
        function: Function,
    },
    /// `get name () { body }`.
    Get {
        is_static: bool,
        name: ClassElementName,
        body: Block,
    },
    /// `set name ( param ) { body }`. The parameter may carry a default
    /// initializer (`set x(v = 1) {}`).
    Set {
        is_static: bool,
        name: ClassElementName,
        param: BindingPattern,
        /// The setter parameter's initializer, if any.
        init: Option<Expr>,
        body: Block,
    },
    /// A class field with an optional initializer.
    Field {
        is_static: bool,
        name: ClassElementName,
        init: Option<Expr>,
        span: Span,
    },
    /// `static { … }`.
    StaticBlock(Block),
}

/// A binding element: `BindingPattern Initializer_opt` (spec 13.2.3.3).
#[derive(Debug, Clone, PartialEq)]
pub struct BindingElement {
    pub pattern: BindingPattern,
    pub init: Option<Expr>,
    /// `...` rest binding (function rest parameter or pattern rest).
    pub rest: bool,
    pub span: Span,
}

/// Binding targets for declarations, parameters, and catch clauses.
#[derive(Debug, Clone, PartialEq)]
pub enum BindingPattern {
    Ident(AtomId),
    Object(Vec<ObjectBindingProperty>),
    Array(Vec<ArrayBindingElement>),
}

/// One position of an array binding pattern (spec 13.2.3.4).
#[derive(Debug, Clone, PartialEq)]
pub enum ArrayBindingElement {
    /// Elision hole: `[a, , b]`.
    Hole,
    Element(BindingElement),
    /// `...rest` — must be the final element.
    Rest(BindingElement),
}

/// `PropertyName : BindingElement` in an object pattern.
#[derive(Debug, Clone, PartialEq)]
pub enum ObjectBindingProperty {
    /// `key : element` or shorthand `key` / `key = default`.
    Property {
        key: PropertyName,
        element: BindingElement,
        span: Span,
    },
    /// `...rest` — must be the final property.
    Rest(BindingElement),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> Span {
        Span::new(0, 1)
    }

    fn id(name: &str) -> AtomId {
        crux::intern_utf8(name)
    }

    fn ident_expr(name: &str) -> Expr {
        Expr {
            span: span(),
            kind: ExprKind::Ident(id(name)),
        }
    }

    #[test]
    fn constructs_literals_and_identifiers() {
        let null = Expr {
            span: span(),
            kind: ExprKind::Literal(Literal::Null),
        };
        assert_eq!(null.kind, ExprKind::Literal(Literal::Null));
        let n = Expr {
            span: span(),
            kind: ExprKind::Literal(Literal::Number(42.0)),
        };
        assert_eq!(n.kind, ExprKind::Literal(Literal::Number(42.0)));
        assert_ne!(n.kind, null.kind);
        assert_eq!(
            ident_expr("x"),
            Expr {
                span: span(),
                kind: ExprKind::Ident(id("x")),
            }
        );
    }

    #[test]
    fn constructs_binary_and_member() {
        let bin = Expr {
            span: span(),
            kind: ExprKind::Binary {
                op: BinaryOp::Add,
                left: Box::new(ident_expr("a")),
                right: Box::new(Expr {
                    span: span(),
                    kind: ExprKind::Literal(Literal::Number(1.0)),
                }),
            },
        };
        assert!(matches!(
            bin.kind,
            ExprKind::Binary {
                op: BinaryOp::Add,
                ..
            }
        ));

        let member = Expr {
            span: span(),
            kind: ExprKind::Member(MemberExpr {
                object: Box::new(ident_expr("a")),
                property: MemberProperty::Name(id("b")),
                optional: false,
                span: span(),
            }),
        };
        assert!(matches!(
            member.kind,
            ExprKind::Member(MemberExpr {
                optional: false,
                ..
            })
        ));
    }

    #[test]
    fn constructs_binding_patterns() {
        let pattern = BindingPattern::Object(vec![ObjectBindingProperty::Property {
            key: PropertyName::Ident(id("x")),
            element: BindingElement {
                pattern: BindingPattern::Ident(id("y")),
                init: None,
                rest: false,
                span: span(),
            },
            span: span(),
        }]);
        let BindingPattern::Object(props) = pattern else {
            panic!("expected object pattern")
        };
        let ObjectBindingProperty::Property { key, element, .. } = &props[0] else {
            panic!("expected property")
        };
        assert_eq!(*key, PropertyName::Ident(id("x")));
        assert_eq!(element.pattern, BindingPattern::Ident(id("y")));

        let rest = BindingPattern::Object(vec![ObjectBindingProperty::Rest(BindingElement {
            pattern: BindingPattern::Ident(id("r")),
            init: None,
            rest: false,
            span: span(),
        })]);
        assert!(matches!(
            rest,
            BindingPattern::Object(v)
                if matches!(v[0], ObjectBindingProperty::Rest(_))
        ));

        let array = BindingPattern::Array(vec![
            ArrayBindingElement::Hole,
            ArrayBindingElement::Element(BindingElement {
                pattern: BindingPattern::Ident(id("a")),
                init: None,
                rest: false,
                span: span(),
            }),
        ]);
        assert!(matches!(
            array,
            BindingPattern::Array(v)
                if v.len() == 2 && matches!(v[0], ArrayBindingElement::Hole)
        ));
    }

    #[test]
    fn constructs_statements_and_program() {
        let stmt = Stmt {
            span: span(),
            kind: StmtKind::Return(Some(ident_expr("x"))),
        };
        assert!(matches!(stmt.kind, StmtKind::Return(Some(_))));

        let if_stmt = Stmt {
            span: span(),
            kind: StmtKind::If {
                test: ident_expr("c"),
                consequent: Box::new(Stmt {
                    span: span(),
                    kind: StmtKind::Empty,
                }),
                alternate: None,
            },
        };
        assert!(matches!(
            if_stmt.kind,
            StmtKind::If {
                alternate: None,
                ..
            }
        ));

        let prog = Program {
            body: vec![stmt, if_stmt],
            span: span(),
        };
        assert_eq!(prog.body.len(), 2);
        assert_eq!(prog.span, span());
    }

    #[test]
    fn template_literal_has_quasis_and_exprs() {
        let tpl = TemplateLiteral {
            quasis: vec![
                TemplateElement {
                    cooked: Some(JsString::from_utf8("a")),
                    raw: JsString::from_utf8("a"),
                    span: span(),
                },
                TemplateElement {
                    cooked: Some(JsString::from_utf8("c")),
                    raw: JsString::from_utf8("c"),
                    span: span(),
                },
            ],
            exprs: vec![ident_expr("b")],
            span: span(),
        };
        assert_eq!(tpl.quasis.len(), 2);
        assert_eq!(tpl.exprs.len(), 1);
    }
}
