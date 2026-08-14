//! Dedicated post-parse early-errors pass (spec ch. 17).
//!
//! The cross-tree rules that cannot be applied while parsing:
//! - label scoping: `ContainsDuplicateLabels`, `ContainsUndefinedBreakTarget`,
//!   `ContainsUndefinedContinueTarget` (sec-syntax-directed-operations-labels)
//! - module exports: unique `ExportedNames` and the `ReferencedBindings`
//!   restrictions (spec 16.2.3)
//! - `arguments` and `await` are early errors in class field initializers
//!   and static blocks (spec 15.7.9)
//! - duplicate `__proto__` data properties in object literals (spec 13.2.5)
//!
//! Functions and classes reset the label scope: their bodies cannot see or
//! be seen by labels in the enclosing statement list.

use std::collections::HashSet;

use crux::{AtomId, JsError, JsString, Span, intern_utf8};
use syntax::keywords::is_reserved_word;
use syntax::{
    Argument, ArrayBindingElement, ArrayElement, ArrowBody, BindingElement, BindingPattern, Block,
    Class, ClassElement, ClassElementName, ExportDecl, ExportName, ExportSpecifier, Expr, ExprKind,
    Module, ModuleItem, ObjectBindingProperty, ObjectLiteral, ObjectProperty, Program,
    PropertyName, Stmt, StmtKind, SwitchCase,
};

/// The label state threaded through a statement tree.
#[derive(Default)]
struct LabelState {
    /// Labels of enclosing labeled statements (for `break label` and
    /// duplicate-label detection).
    entries: Vec<LabelEntry>,
    /// Loops and switches on the path (for unlabeled `break`).
    breakable: usize,
    /// Loops on the path (for unlabeled `continue`).
    loops: usize,
}

struct LabelEntry {
    name: AtomId,
    /// Whether the labeled statement (unwrapping nested labels) is an
    /// iteration statement; `continue label` requires this.
    is_iteration: bool,
}

fn error_at(span: Span, message: &str) -> JsError {
    JsError::new(crux::ErrorKind::SyntaxError, message.into()).with_span(span)
}

/// Checks the statement list of a Script.
pub(crate) fn check_script(program: &Program) -> Result<(), JsError> {
    check_stmts(&program.body, &mut LabelState::default())
}

/// Checks a Module's statements, labels, and exports.
pub(crate) fn check_module(module: &Module) -> Result<(), JsError> {
    let mut labels = LabelState::default();
    for item in &module.body {
        match item {
            ModuleItem::Stmt(stmt) => check_stmt(stmt, &mut labels)?,
            ModuleItem::Import(_) => {}
            ModuleItem::Export(decl) => check_export(decl, &mut labels)?,
        }
    }
    check_exported_names(module)
}

fn check_stmts(stmts: &[Stmt], labels: &mut LabelState) -> Result<(), JsError> {
    for stmt in stmts {
        check_stmt(stmt, labels)?;
    }
    Ok(())
}

fn check_stmt(stmt: &Stmt, labels: &mut LabelState) -> Result<(), JsError> {
    match &stmt.kind {
        StmtKind::Block(block) => check_stmts(&block.stmts, labels),
        StmtKind::Empty | StmtKind::Debugger => Ok(()),
        StmtKind::Expr(expr) => check_expr(expr, labels),
        StmtKind::If {
            test,
            consequent,
            alternate,
        } => {
            check_expr(test, labels)?;
            check_stmt(consequent, labels)?;
            if let Some(alt) = alternate {
                check_stmt(alt, labels)?;
            }
            Ok(())
        }
        StmtKind::VarDecl { decls, .. } | StmtKind::UsingDecl { decls, .. } => {
            for decl in decls {
                if let Some(init) = &decl.init {
                    check_expr(init, labels)?;
                }
            }
            Ok(())
        }
        StmtKind::FunctionDecl(f) => check_function(f),
        StmtKind::ClassDecl(c) => check_class(c, labels),
        StmtKind::Return(Some(expr)) | StmtKind::Throw(expr) => check_expr(expr, labels),
        StmtKind::Return(None) => Ok(()),
        StmtKind::Labeled { label, body } => check_labeled(stmt.span, *label, body, labels),
        StmtKind::Break(Some(label)) => {
            if !labels.entries.iter().any(|e| e.name == *label) {
                return Err(error_at(stmt.span, "Undefined label"));
            }
            Ok(())
        }
        StmtKind::Break(None) => {
            if labels.breakable == 0 {
                return Err(error_at(stmt.span, "Illegal break statement"));
            }
            Ok(())
        }
        StmtKind::Continue(Some(label)) => {
            if !labels
                .entries
                .iter()
                .any(|e| e.name == *label && e.is_iteration)
            {
                return Err(error_at(stmt.span, "Undefined label"));
            }
            Ok(())
        }
        StmtKind::Continue(None) => {
            if labels.loops == 0 {
                return Err(error_at(stmt.span, "Illegal continue statement"));
            }
            Ok(())
        }
        StmtKind::While { test, body } => {
            check_expr(test, labels)?;
            check_loop_body(body, labels)
        }
        StmtKind::DoWhile { body, test } => {
            check_loop_body(body, labels)?;
            check_expr(test, labels)
        }
        StmtKind::For {
            init,
            test,
            update,
            body,
        } => {
            check_for_init(init, labels)?;
            if let Some(t) = test {
                check_expr(t, labels)?;
            }
            if let Some(u) = update {
                check_expr(u, labels)?;
            }
            check_loop_body(body, labels)
        }
        StmtKind::ForIn { left, right, body } => {
            check_for_binding(left, labels)?;
            check_expr(right, labels)?;
            check_loop_body(body, labels)
        }
        StmtKind::ForOf {
            left, right, body, ..
        } => {
            check_for_binding(left, labels)?;
            check_expr(right, labels)?;
            check_loop_body(body, labels)
        }
        StmtKind::Try {
            block,
            handler,
            finalizer,
        } => {
            check_stmts(&block.stmts, labels)?;
            if let Some(handler) = handler {
                if let Some(param) = &handler.param {
                    check_binding_pattern(param, labels)?;
                }
                check_stmts(&handler.body.stmts, labels)?;
            }
            if let Some(finalizer) = finalizer {
                check_stmts(&finalizer.stmts, labels)?;
            }
            Ok(())
        }
        StmtKind::Switch {
            discriminant,
            cases,
        } => {
            check_expr(discriminant, labels)?;
            labels.breakable += 1;
            let result = check_switch_cases(cases, labels);
            labels.breakable -= 1;
            result
        }
        StmtKind::With { object, body } => {
            check_expr(object, labels)?;
            check_stmt(body, labels)
        }
    }
}

fn check_loop_body(body: &Stmt, labels: &mut LabelState) -> Result<(), JsError> {
    labels.breakable += 1;
    labels.loops += 1;
    let result = check_stmt(body, labels);
    labels.loops -= 1;
    labels.breakable -= 1;
    result
}

fn check_labeled(
    span: Span,
    label: AtomId,
    body: &Stmt,
    labels: &mut LabelState,
) -> Result<(), JsError> {
    if labels.entries.iter().any(|e| e.name == label) {
        return Err(error_at(span, "Label has already been declared"));
    }
    let is_iteration = iteration_target(body);
    labels.entries.push(LabelEntry {
        name: label,
        is_iteration,
    });
    let result = check_stmt(body, labels);
    labels.entries.pop();
    result
}

/// Whether the statement, unwrapping nested labels, is an iteration
/// statement — labels on such a chain satisfy `continue label`.
fn iteration_target(mut stmt: &Stmt) -> bool {
    loop {
        match &stmt.kind {
            StmtKind::Labeled { body, .. } => stmt = body,
            StmtKind::While { .. }
            | StmtKind::DoWhile { .. }
            | StmtKind::For { .. }
            | StmtKind::ForIn { .. }
            | StmtKind::ForOf { .. } => return true,
            _ => return false,
        }
    }
}

fn check_switch_cases(cases: &[SwitchCase], labels: &mut LabelState) -> Result<(), JsError> {
    for case in cases {
        if let Some(test) = &case.test {
            check_expr(test, labels)?;
        }
        check_stmts(&case.consequent, labels)?;
    }
    Ok(())
}

fn check_for_init(init: &Option<syntax::ForInit>, labels: &mut LabelState) -> Result<(), JsError> {
    match init {
        Some(syntax::ForInit::Expr(expr)) => check_expr(expr, labels),
        Some(syntax::ForInit::VarDecl { decls, .. }) => {
            for decl in decls {
                if let Some(init) = &decl.init {
                    check_expr(init, labels)?;
                }
            }
            Ok(())
        }
        None => Ok(()),
    }
}

fn check_for_binding(binding: &syntax::ForBinding, labels: &mut LabelState) -> Result<(), JsError> {
    match binding {
        syntax::ForBinding::Expr(expr) => check_expr(expr, labels),
        syntax::ForBinding::VarDecl { init, .. } => {
            if let Some(init) = init {
                check_expr(init, labels)?;
            }
            Ok(())
        }
    }
}

/// A function body starts a fresh label scope: `break`/`continue` cannot
/// reference labels outside, and its own labels cannot clash with them.
pub(crate) fn check_function(f: &syntax::Function) -> Result<(), JsError> {
    let mut fresh = LabelState::default();
    check_binding_elements(&f.params, &mut fresh)?;
    check_stmts(&f.body.stmts, &mut fresh)
}

fn check_function_body(body: &Block) -> Result<(), JsError> {
    check_stmts(&body.stmts, &mut LabelState::default())
}

fn check_binding_elements(
    elements: &[BindingElement],
    labels: &mut LabelState,
) -> Result<(), JsError> {
    for element in elements {
        check_binding_element(element, labels)?;
    }
    Ok(())
}

fn check_binding_element(element: &BindingElement, labels: &mut LabelState) -> Result<(), JsError> {
    check_binding_pattern(&element.pattern, labels)?;
    if let Some(init) = &element.init {
        check_expr(init, labels)?;
    }
    Ok(())
}

fn check_binding_pattern(pattern: &BindingPattern, labels: &mut LabelState) -> Result<(), JsError> {
    match pattern {
        BindingPattern::Ident(_) => Ok(()),
        BindingPattern::Object(props) => {
            for prop in props {
                match prop {
                    ObjectBindingProperty::Property { element, .. }
                    | ObjectBindingProperty::Rest(element) => {
                        check_binding_element(element, labels)?;
                    }
                }
            }
            Ok(())
        }
        BindingPattern::Array(elements) => {
            for element in elements {
                match element {
                    ArrayBindingElement::Hole => {}
                    ArrayBindingElement::Element(e) | ArrayBindingElement::Rest(e) => {
                        check_binding_element(e, labels)?;
                    }
                }
            }
            Ok(())
        }
    }
}

fn check_class(c: &Class, labels: &mut LabelState) -> Result<(), JsError> {
    if let Some(heritage) = &c.heritage {
        check_expr(heritage, labels)?;
    }
    for element in &c.elements {
        match element {
            ClassElement::Method { name, function, .. } => {
                check_class_element_name(name, labels)?;
                check_function(function)?;
            }
            ClassElement::Get { name, body, .. } => {
                check_class_element_name(name, labels)?;
                check_function_body(body)?;
            }
            ClassElement::Set {
                name, param, body, ..
            } => {
                check_class_element_name(name, labels)?;
                let mut fresh = LabelState::default();
                check_binding_pattern(param, &mut fresh)?;
                check_function_body(body)?;
            }
            ClassElement::Field { name, init, .. } => {
                check_class_element_name(name, labels)?;
                if let Some(init) = init {
                    check_field_initializer(init)?;
                }
            }
            ClassElement::StaticBlock(block) => check_static_block(block)?,
        }
    }
    Ok(())
}

fn check_class_element_name(
    name: &ClassElementName,
    labels: &mut LabelState,
) -> Result<(), JsError> {
    if let ClassElementName::Property(PropertyName::Computed(expr)) = name {
        check_expr(expr, labels)?;
    }
    Ok(())
}

/// A field initializer is evaluated in the constructor's environment but
/// `arguments` is an early error there (spec 15.7.9); nested arrows and
/// functions have their own `arguments` and are checked with a fresh label
/// scope.
fn check_field_initializer(init: &Expr) -> Result<(), JsError> {
    if contains_arguments(init) {
        return Err(error_at(
            init.span,
            "'arguments' is not allowed in class field initializers",
        ));
    }
    check_expr(init, &mut LabelState::default())
}

/// A class static block is strict, return-less, and forbids `arguments` and
/// `await` (spec 15.7.11).
fn check_static_block(block: &Block) -> Result<(), JsError> {
    if contains_arguments_in_stmts(&block.stmts) {
        return Err(error_at(
            block.span,
            "'arguments' is not allowed in class static blocks",
        ));
    }
    if contains_await_in_stmts(&block.stmts) {
        return Err(error_at(
            block.span,
            "'await' is not allowed in class static blocks",
        ));
    }
    check_stmts(&block.stmts, &mut LabelState::default())
}

/// Walks every expression reachable without crossing a function, arrow, or
/// class boundary (the spec's `Contains` rules do not look into function
/// definitions, and arrows have their own `arguments`/`await`).
pub(crate) fn walk_exprs(expr: &Expr, visit: &mut impl FnMut(&Expr)) {
    visit(expr);
    match &expr.kind {
        ExprKind::Literal(_)
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::MetaProperty { .. }
        | ExprKind::Function(_)
        | ExprKind::Class(_)
        | ExprKind::Arrow { .. } => {}
        ExprKind::PrivateIn { object, .. } => walk_exprs(object, visit),
        ExprKind::Array(array) => {
            for element in &array.elements {
                match element {
                    ArrayElement::Expr(e) | ArrayElement::Spread(e) => walk_exprs(e, visit),
                    ArrayElement::Hole => {}
                }
            }
        }
        ExprKind::Object(object) => {
            for prop in &object.props {
                match prop {
                    ObjectProperty::Init { key, value, .. } => {
                        walk_property_name(key, visit);
                        walk_exprs(value, visit);
                    }
                    ObjectProperty::Method { key, .. }
                    | ObjectProperty::Get { key, .. }
                    | ObjectProperty::Set { key, .. } => walk_property_name(key, visit),
                    ObjectProperty::Spread(e) => walk_exprs(e, visit),
                }
            }
        }
        ExprKind::Unary { operand, .. } => walk_exprs(operand, visit),
        ExprKind::Update { target, .. } => walk_exprs(target, visit),
        ExprKind::Binary { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            walk_exprs(left, visit);
            walk_exprs(right, visit);
        }
        ExprKind::Assign { target, value, .. } => {
            walk_exprs(target, visit);
            walk_exprs(value, visit);
        }
        ExprKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            walk_exprs(test, visit);
            walk_exprs(consequent, visit);
            walk_exprs(alternate, visit);
        }
        ExprKind::Call(call) => {
            walk_exprs(&call.callee, visit);
            for arg in &call.args {
                walk_argument(arg, visit);
            }
        }
        ExprKind::New(new) => {
            walk_exprs(&new.callee, visit);
            for arg in &new.args {
                walk_argument(arg, visit);
            }
        }
        ExprKind::Member(member) => {
            walk_exprs(&member.object, visit);
            if let syntax::MemberProperty::Computed(index) = &member.property {
                walk_exprs(index, visit);
            }
        }
        ExprKind::TaggedTemplate { tag, quasi } => {
            walk_exprs(tag, visit);
            for e in &quasi.exprs {
                walk_exprs(e, visit);
            }
        }
        ExprKind::Template(template) => {
            for e in &template.exprs {
                walk_exprs(e, visit);
            }
        }
        ExprKind::Paren(inner) => walk_exprs(inner, visit),
        ExprKind::Sequence(exprs) => {
            for e in exprs {
                walk_exprs(e, visit);
            }
        }
        ExprKind::Yield { argument, .. } => {
            if let Some(argument) = argument {
                walk_exprs(argument, visit);
            }
        }
        ExprKind::Await(operand) => walk_exprs(operand, visit),
        ExprKind::ImportCall { specifier, options } => {
            walk_exprs(specifier, visit);
            if let Some(options) = options {
                walk_exprs(options, visit);
            }
        }
    }
}

fn walk_argument(argument: &Argument, visit: &mut impl FnMut(&Expr)) {
    match argument {
        Argument::Expr(e) | Argument::Spread(e) => walk_exprs(e, visit),
    }
}

fn walk_property_name(name: &PropertyName, visit: &mut impl FnMut(&Expr)) {
    if let PropertyName::Computed(expr) = name {
        walk_exprs(expr, visit);
    }
}

fn contains_arguments(expr: &Expr) -> bool {
    let mut found = false;
    walk_exprs(expr, &mut |e| {
        if matches!(e.kind, ExprKind::Ident(atom) if atom == intern_utf8("arguments")) {
            found = true;
        }
    });
    found
}

fn contains_arguments_in_stmts(stmts: &[Stmt]) -> bool {
    let mut found = false;
    walk_stmts(stmts, &mut |e| {
        if matches!(e.kind, ExprKind::Ident(atom) if atom == intern_utf8("arguments")) {
            found = true;
        }
    });
    found
}

fn contains_await_in_stmts(stmts: &[Stmt]) -> bool {
    let mut found = false;
    walk_stmts(stmts, &mut |e| {
        if matches!(e.kind, ExprKind::Await(_))
            || matches!(e.kind, ExprKind::Ident(atom) if atom == intern_utf8("await"))
        {
            found = true;
        }
    });
    found
}

/// Collects the expressions of a statement list without crossing function,
/// arrow, or class boundaries.
fn walk_stmts(stmts: &[Stmt], visit: &mut impl FnMut(&Expr)) {
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::Block(block) => walk_stmts(&block.stmts, visit),
            StmtKind::Expr(expr) => walk_exprs(expr, visit),
            StmtKind::If {
                test,
                consequent,
                alternate,
            } => {
                walk_exprs(test, visit);
                walk_stmts(std::slice::from_ref(consequent), visit);
                if let Some(alt) = alternate {
                    walk_stmts(std::slice::from_ref(alt), visit);
                }
            }
            StmtKind::VarDecl { decls, .. } | StmtKind::UsingDecl { decls, .. } => {
                for decl in decls {
                    if let Some(init) = &decl.init {
                        walk_exprs(init, visit);
                    }
                }
            }
            StmtKind::Return(Some(expr)) | StmtKind::Throw(expr) => walk_exprs(expr, visit),
            StmtKind::Return(None) => {}
            StmtKind::While { test, body } => {
                walk_exprs(test, visit);
                walk_stmts(std::slice::from_ref(body), visit);
            }
            StmtKind::DoWhile { body, test } => {
                walk_stmts(std::slice::from_ref(body), visit);
                walk_exprs(test, visit);
            }
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => {
                walk_for_init(init, visit);
                if let Some(t) = test {
                    walk_exprs(t, visit);
                }
                if let Some(u) = update {
                    walk_exprs(u, visit);
                }
                walk_stmts(std::slice::from_ref(body), visit);
            }
            StmtKind::ForIn { left, right, body }
            | StmtKind::ForOf {
                left, right, body, ..
            } => {
                walk_for_binding(left, visit);
                walk_exprs(right, visit);
                walk_stmts(std::slice::from_ref(body), visit);
            }
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => {
                walk_stmts(&block.stmts, visit);
                if let Some(handler) = handler {
                    walk_stmts(&handler.body.stmts, visit);
                }
                if let Some(finalizer) = finalizer {
                    walk_stmts(&finalizer.stmts, visit);
                }
            }
            StmtKind::Switch {
                discriminant,
                cases,
            } => {
                walk_exprs(discriminant, visit);
                for case in cases {
                    if let Some(test) = &case.test {
                        walk_exprs(test, visit);
                    }
                    walk_stmts(&case.consequent, visit);
                }
            }
            StmtKind::With { object, body } => {
                walk_exprs(object, visit);
                walk_stmts(std::slice::from_ref(body), visit);
            }
            StmtKind::Labeled { body, .. } => walk_stmts(std::slice::from_ref(body), visit),
            StmtKind::FunctionDecl(_) | StmtKind::ClassDecl(_) => {}
            StmtKind::Empty | StmtKind::Debugger | StmtKind::Break(_) | StmtKind::Continue(_) => {}
        }
    }
}

fn walk_for_init(init: &Option<syntax::ForInit>, visit: &mut impl FnMut(&Expr)) {
    match init {
        Some(syntax::ForInit::Expr(expr)) => walk_exprs(expr, visit),
        Some(syntax::ForInit::VarDecl { decls, .. }) => {
            for decl in decls {
                if let Some(init) = &decl.init {
                    walk_exprs(init, visit);
                }
            }
        }
        None => {}
    }
}

fn walk_for_binding(binding: &syntax::ForBinding, visit: &mut impl FnMut(&Expr)) {
    match binding {
        syntax::ForBinding::Expr(expr) => walk_exprs(expr, visit),
        syntax::ForBinding::VarDecl { init, .. } => {
            if let Some(init) = init {
                walk_exprs(init, visit);
            }
        }
    }
}

// ---- module exports (spec 16.2.3) ----

/// Validates the exports of a module: each exported name may appear only
/// once (star re-exports contribute no names), and the local names of
/// `export { … }` must be plain identifiers.
fn check_exported_names(module: &Module) -> Result<(), JsError> {
    let mut seen: HashSet<JsString> = HashSet::new();
    for item in &module.body {
        let ModuleItem::Export(decl) = item else {
            continue;
        };
        match decl {
            ExportDecl::Named { specifiers, span } => {
                for spec in specifiers {
                    let local = export_specifier_local(spec);
                    match local {
                        ExportName::Str(_) => {
                            return Err(error_at(*span, "Export specifier must be an identifier"));
                        }
                        ExportName::Ident(atom) => {
                            if is_reserved_word(*atom) {
                                return Err(error_at(*span, "Unexpected reserved word in export"));
                            }
                        }
                    }
                    register_export_name(&mut seen, export_specifier_exported(spec), *span)?;
                }
            }
            ExportDecl::From {
                specifiers,
                namespace,
                span,
                ..
            } => {
                if let Some(ns) = namespace {
                    register_export_name(&mut seen, ns.clone(), *span)?;
                }
                for spec in specifiers {
                    register_export_name(&mut seen, export_specifier_exported(spec), *span)?;
                }
            }
            ExportDecl::Declaration(stmt) => {
                for name in declaration_bound_names(stmt) {
                    register_export_string(&mut seen, name, stmt.span)?;
                }
            }
            ExportDecl::Default(_) => {
                register_export_string(&mut seen, JsString::from_utf8("default"), item_span(decl))?
            }
        }
    }
    Ok(())
}

fn item_span(decl: &ExportDecl) -> Span {
    match decl {
        ExportDecl::Named { span, .. } | ExportDecl::From { span, .. } => *span,
        ExportDecl::Declaration(stmt) => stmt.span,
        ExportDecl::Default(inner) => match &**inner {
            syntax::ExportDefault::Function(f) => f.span,
            syntax::ExportDefault::Class(c) => c.span,
            syntax::ExportDefault::Expr(e) => e.span,
        },
    }
}

fn export_specifier_local(spec: &ExportSpecifier) -> &ExportName {
    match spec {
        ExportSpecifier::Same(name) => name,
        ExportSpecifier::Alias { local, .. } => local,
    }
}

fn export_specifier_exported(spec: &ExportSpecifier) -> ExportName {
    match spec {
        ExportSpecifier::Same(name) => name.clone(),
        ExportSpecifier::Alias { exported, .. } => exported.clone(),
    }
}

fn register_export_name(
    seen: &mut HashSet<JsString>,
    name: ExportName,
    span: Span,
) -> Result<(), JsError> {
    match name {
        ExportName::Ident(atom) => register_export_string(seen, crux::lookup(atom), span),
        ExportName::Str(value) => register_export_string(seen, value, span),
    }
}

fn register_export_string(
    seen: &mut HashSet<JsString>,
    name: JsString,
    span: Span,
) -> Result<(), JsError> {
    if !seen.insert(name) {
        return Err(error_at(span, "Duplicate export"));
    }
    Ok(())
}

/// The names bound by an `export var/let/const/function/class` declaration.
fn declaration_bound_names(stmt: &Stmt) -> Vec<JsString> {
    let mut out = Vec::new();
    match &stmt.kind {
        StmtKind::VarDecl { decls, .. } | StmtKind::UsingDecl { decls, .. } => {
            for decl in decls {
                collect_pattern_names(&decl.pattern, &mut out);
            }
        }
        StmtKind::FunctionDecl(f) => {
            if let Some(name) = f.name {
                out.push(crux::lookup(name));
            }
        }
        StmtKind::ClassDecl(c) => {
            if let Some(name) = c.name {
                out.push(crux::lookup(name));
            }
        }
        _ => {}
    }
    out
}

fn collect_pattern_names(pattern: &BindingPattern, out: &mut Vec<JsString>) {
    match pattern {
        BindingPattern::Ident(name) => out.push(crux::lookup(*name)),
        BindingPattern::Object(props) => {
            for prop in props {
                match prop {
                    ObjectBindingProperty::Property { element, .. }
                    | ObjectBindingProperty::Rest(element) => {
                        collect_pattern_names(&element.pattern, out);
                    }
                }
            }
        }
        BindingPattern::Array(elements) => {
            for element in elements {
                match element {
                    ArrayBindingElement::Hole => {}
                    ArrayBindingElement::Element(e) | ArrayBindingElement::Rest(e) => {
                        collect_pattern_names(&e.pattern, out);
                    }
                }
            }
        }
    }
}

// ---- expressions (spec 13.2.5) ----

fn check_expr(expr: &Expr, labels: &mut LabelState) -> Result<(), JsError> {
    match &expr.kind {
        ExprKind::Function(f) => check_function(f),
        ExprKind::Class(c) => check_class(c, labels),
        ExprKind::Arrow { params, body, .. } => {
            let mut fresh = LabelState::default();
            check_binding_elements(params, &mut fresh)?;
            match body {
                ArrowBody::Expr(expr) => check_expr(expr, &mut fresh),
                ArrowBody::Block(block) => check_stmts(&block.stmts, &mut fresh),
            }
        }
        ExprKind::Object(object) => {
            check_object_literal(object, labels)?;
            Ok(())
        }
        ExprKind::Array(array) => {
            for element in &array.elements {
                match element {
                    ArrayElement::Expr(e) | ArrayElement::Spread(e) => check_expr(e, labels)?,
                    ArrayElement::Hole => {}
                }
            }
            Ok(())
        }
        ExprKind::Unary { operand, .. } => check_expr(operand, labels),
        ExprKind::Update { target, .. } => check_expr(target, labels),
        ExprKind::Binary { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            check_expr(left, labels)?;
            check_expr(right, labels)
        }
        ExprKind::PrivateIn { object, .. } => check_expr(object, labels),
        ExprKind::Assign { target, value, .. } => {
            check_expr(target, labels)?;
            check_expr(value, labels)
        }
        ExprKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            check_expr(test, labels)?;
            check_expr(consequent, labels)?;
            check_expr(alternate, labels)
        }
        ExprKind::Call(call) => {
            check_expr(&call.callee, labels)?;
            for arg in &call.args {
                match arg {
                    Argument::Expr(e) | Argument::Spread(e) => check_expr(e, labels)?,
                }
            }
            Ok(())
        }
        ExprKind::New(new) => {
            check_expr(&new.callee, labels)?;
            for arg in &new.args {
                match arg {
                    Argument::Expr(e) | Argument::Spread(e) => check_expr(e, labels)?,
                }
            }
            Ok(())
        }
        ExprKind::Member(member) => {
            check_expr(&member.object, labels)?;
            if let syntax::MemberProperty::Computed(index) = &member.property {
                check_expr(index, labels)?;
            }
            Ok(())
        }
        ExprKind::TaggedTemplate { tag, quasi } => {
            check_expr(tag, labels)?;
            for e in &quasi.exprs {
                check_expr(e, labels)?;
            }
            Ok(())
        }
        ExprKind::Template(template) => {
            for e in &template.exprs {
                check_expr(e, labels)?;
            }
            Ok(())
        }
        ExprKind::Paren(inner) => check_expr(inner, labels),
        ExprKind::Sequence(exprs) => {
            for e in exprs {
                check_expr(e, labels)?;
            }
            Ok(())
        }
        ExprKind::Yield { argument, .. } => {
            if let Some(argument) = argument {
                check_expr(argument, labels)?;
            }
            Ok(())
        }
        ExprKind::Await(operand) => check_expr(operand, labels),
        ExprKind::ImportCall { specifier, options } => {
            check_expr(specifier, labels)?;
            if let Some(options) = options {
                check_expr(options, labels)?;
            }
            Ok(())
        }
        ExprKind::Literal(_)
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::MetaProperty { .. } => Ok(()),
    }
}

/// `__proto__` may appear as a data property only once (spec 13.2.5 early
/// errors); shorthand and method entries do not count.
fn check_object_literal(object: &ObjectLiteral, labels: &mut LabelState) -> Result<(), JsError> {
    let mut proto_count = 0usize;
    for prop in &object.props {
        if is_proto_data_property(prop) {
            proto_count += 1;
            if proto_count > 1 {
                let span = match prop {
                    ObjectProperty::Init { value, .. } => value.span,
                    _ => object.span,
                };
                return Err(error_at(
                    span,
                    "Duplicate __proto__ fields are not allowed in object literals",
                ));
            }
        }
    }
    for prop in &object.props {
        match prop {
            ObjectProperty::Init { key, value, .. } => {
                if let PropertyName::Computed(computed) = key {
                    check_expr(computed, labels)?;
                }
                check_expr(value, labels)?;
            }
            ObjectProperty::Method { key, function } => {
                if let PropertyName::Computed(computed) = key {
                    check_expr(computed, labels)?;
                }
                check_function(function)?;
            }
            ObjectProperty::Get { key, body } | ObjectProperty::Set { key, body, .. } => {
                if let PropertyName::Computed(computed) = key {
                    check_expr(computed, labels)?;
                }
                check_function_body(body)?;
            }
            ObjectProperty::Spread(e) => check_expr(e, labels)?,
        }
    }
    Ok(())
}

/// Whether a property is a `PropertyName : AssignmentExpression` entry whose
/// name is `__proto__` (shorthand entries are not of that production form).
fn is_proto_data_property(prop: &ObjectProperty) -> bool {
    let ObjectProperty::Init {
        key,
        value: _,
        shorthand,
    } = prop
    else {
        return false;
    };
    if *shorthand {
        return false;
    }
    match key {
        PropertyName::Ident(atom) => *atom == intern_utf8("__proto__"),
        PropertyName::Str(s) => s == &JsString::from_utf8("__proto__"),
        _ => false,
    }
}

/// Whether a statement list's expressions (not crossing function, arrow, or
/// class boundaries) contain a given pattern.
fn check_export(decl: &ExportDecl, labels: &mut LabelState) -> Result<(), JsError> {
    match decl {
        ExportDecl::Named { .. } | ExportDecl::From { .. } => Ok(()),
        ExportDecl::Declaration(stmt) => check_stmt(stmt, labels),
        ExportDecl::Default(inner) => match &**inner {
            syntax::ExportDefault::Function(f) => check_function(f),
            syntax::ExportDefault::Class(c) => check_class(c, labels),
            syntax::ExportDefault::Expr(e) => check_expr(e, labels),
        },
    }
}
