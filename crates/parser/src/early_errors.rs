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
    Function, ImportEntry, Module, ModuleItem, ObjectBindingProperty, ObjectLiteral,
    ObjectProperty, Program, PropertyName, Stmt, StmtKind, SwitchCase, VarDeclKind,
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
    check_stmts(&program.body, &mut LabelState::default())?;
    check_private_names(program)
}

/// Checks the statement list of eval code (spec 19.2.1.1): the
/// AllPrivateIdentifiersValid walk is seeded with the caller's private names,
/// so `#name` in eval resolves against the inherited private environment
/// while nested classes still validate their own declarations.
pub(crate) fn check_script_eval(
    program: &Program,
    caller_private_names: &[AtomId],
) -> Result<(), JsError> {
    check_stmts(&program.body, &mut LabelState::default())?;
    let mut env = Vec::new();
    env.push(caller_private_names.iter().copied().collect());
    check_private_stmts(&program.body, &mut env)
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
    check_module_declarations(module)?;
    check_exported_names(module)?;
    check_private_names_module(module)
}

/// The ModuleItemList/Module early errors (spec 16.2.1.1, 16.2.2.1): at the
/// module top level, function/class declarations are lexical, so duplicate
/// lexical names are errors, a lexical name may not collide with a var name,
/// and an `export { local }` name must be a declared binding.
fn check_module_declarations(module: &Module) -> Result<(), JsError> {
    let mut lexical: HashSet<JsString> = HashSet::new();
    let mut vars: HashSet<JsString> = HashSet::new();
    let mut exported_bindings: Vec<(JsString, Span)> = Vec::new();
    for item in &module.body {
        match item {
            ModuleItem::Import(import) => {
                for entry in &import.entries {
                    let local = match entry {
                        ImportEntry::Namespace { local, .. }
                        | ImportEntry::Default { local, .. }
                        | ImportEntry::Named { local, .. } => crux::lookup(*local),
                    };
                    if !lexical.insert(local) {
                        return Err(error_at(import.span, "Duplicate declaration"));
                    }
                }
            }
            ModuleItem::Stmt(stmt) => {
                register_module_statement_names(stmt, &mut lexical, &mut vars, stmt.span)?;
            }
            ModuleItem::Export(decl) => match decl {
                ExportDecl::Declaration(stmt) => {
                    register_module_statement_names(stmt, &mut lexical, &mut vars, stmt.span)?;
                    for name in declaration_bound_names(stmt) {
                        exported_bindings.push((name, stmt.span));
                    }
                }
                ExportDecl::Default(inner) => match &**inner {
                    syntax::ExportDefault::Function(f) => {
                        if let Some(name) = f.name {
                            register_lexical_name(&mut lexical, &vars, crux::lookup(name), f.span)?;
                        }
                    }
                    syntax::ExportDefault::Class(c) => {
                        if let Some(name) = c.name {
                            register_lexical_name(&mut lexical, &vars, crux::lookup(name), c.span)?;
                        }
                    }
                    syntax::ExportDefault::Expr(_) => {}
                },
                ExportDecl::Named { specifiers, span } => {
                    // A local export name must be a declared binding (spec
                    // 16.2.2.1 step 3); `export … from 'm'` re-exports and is
                    // exempt.
                    for spec in specifiers {
                        let local = export_specifier_local(spec);
                        let ExportName::Ident(atom) = local else {
                            continue;
                        };
                        exported_bindings.push((crux::lookup(*atom), *span));
                    }
                }
                ExportDecl::From { .. } => {}
            },
        }
    }
    for (name, span) in exported_bindings {
        if !lexical.contains(&name) && !vars.contains(&name) {
            return Err(error_at(span, "Export of an undeclared name"));
        }
    }
    Ok(())
}

fn register_module_statement_names(
    stmt: &Stmt,
    lexical: &mut HashSet<JsString>,
    vars: &mut HashSet<JsString>,
    span: Span,
) -> Result<(), JsError> {
    match &stmt.kind {
        StmtKind::VarDecl { kind, decls, .. } => {
            for decl in decls {
                let mut names = Vec::new();
                collect_pattern_names(&decl.pattern, &mut names);
                if *kind == VarDeclKind::Var {
                    for name in names {
                        if lexical.contains(&name) {
                            return Err(error_at(span, "Duplicate declaration"));
                        }
                        vars.insert(name);
                    }
                } else {
                    for name in names {
                        register_lexical_name(lexical, vars, name, span)?;
                    }
                }
            }
        }
        StmtKind::UsingDecl { decls, .. } => {
            for decl in decls {
                let mut names = Vec::new();
                collect_pattern_names(&decl.pattern, &mut names);
                for name in names {
                    register_lexical_name(lexical, vars, name, span)?;
                }
            }
        }
        StmtKind::FunctionDecl(f) => {
            if let Some(name) = f.name {
                register_lexical_name(lexical, vars, crux::lookup(name), span)?;
            }
        }
        StmtKind::ClassDecl(c) => {
            if let Some(name) = c.name {
                register_lexical_name(lexical, vars, crux::lookup(name), span)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn register_lexical_name(
    lexical: &mut HashSet<JsString>,
    vars: &HashSet<JsString>,
    name: JsString,
    span: Span,
) -> Result<(), JsError> {
    if lexical.contains(&name) || vars.contains(&name) {
        return Err(error_at(span, "Duplicate declaration"));
    }
    lexical.insert(name);
    Ok(())
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
                name,
                param,
                init,
                body,
                ..
            } => {
                check_class_element_name(name, labels)?;
                let mut fresh = LabelState::default();
                check_binding_pattern(param, &mut fresh)?;
                if let Some(init) = init {
                    check_expr(init, &mut fresh)?;
                }
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
/// `arguments` is an early error there (spec 15.7.9); nested arrows inherit
/// `arguments` and are checked too, while nested functions have their own
/// `arguments` (spec 15.7.9 ContainsArguments).
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
        ExprKind::ImportCall {
            specifier, options, ..
        } => {
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
    walk_contained_exprs(
        expr,
        &mut |e| {
            if matches!(e.kind, ExprKind::Ident(atom) if atom == intern_utf8("arguments")) {
                found = true;
            }
        },
        true,
    );
    found
}

fn contains_arguments_in_stmts(stmts: &[Stmt]) -> bool {
    let mut found = false;
    walk_contained_stmts(
        stmts,
        &mut |e| {
            if matches!(e.kind, ExprKind::Ident(atom) if atom == intern_utf8("arguments")) {
                found = true;
            }
        },
        true,
    );
    found
}

fn contains_await_in_stmts(stmts: &[Stmt]) -> bool {
    let mut found = false;
    walk_contained_stmts(
        stmts,
        &mut |e| {
            if matches!(e.kind, ExprKind::Await(_))
                || matches!(e.kind, ExprKind::Ident(atom) if atom == intern_utf8("await"))
            {
                found = true;
            }
        },
        false,
    );
    found
}

/// Walks the expressions reachable from a field initializer or a class
/// static block without crossing a function or class-method boundary, for
/// the spec `ContainsArguments`/`ContainsAwait` checks (15.7.9/15.7.11):
/// nested functions and method bodies are opaque, and a nested class
/// contributes its heritage, computed property names, and field initializers
/// but not its method bodies or static blocks. `through_arrows` selects the
/// per-check arrow behavior: `ContainsArguments` recurses through arrows
/// (they inherit the enclosing `arguments`), while the static block's
/// `Contains await` treats them as opaque (spec sec-static-semantics-contains).
fn walk_contained_exprs(expr: &Expr, visit: &mut impl FnMut(&Expr), through_arrows: bool) {
    visit(expr);
    match &expr.kind {
        ExprKind::Literal(_)
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::MetaProperty { .. }
        | ExprKind::Function(_) => {}
        ExprKind::Class(class) => walk_contained_class(class, visit, through_arrows),
        ExprKind::Arrow { params, body, .. } => {
            // The spec `Contains` operation makes arrows opaque to every
            // symbol but `new.target`/`super`/`this`, so the static-block
            // `await` check skips arrow bodies and params (an arrow's own
            // params-await is its own early error). `arguments` recurses
            // through arrows: they inherit the enclosing `arguments`.
            if through_arrows {
                for param in params {
                    if let Some(init) = &param.init {
                        walk_contained_exprs(init, visit, through_arrows);
                    }
                }
                match body {
                    ArrowBody::Expr(expr) => walk_contained_exprs(expr, visit, through_arrows),
                    ArrowBody::Block(block) => {
                        walk_contained_stmts(&block.stmts, visit, through_arrows)
                    }
                }
            }
        }
        ExprKind::PrivateIn { object, .. } => walk_contained_exprs(object, visit, through_arrows),
        ExprKind::Array(array) => {
            for element in &array.elements {
                match element {
                    ArrayElement::Expr(e) | ArrayElement::Spread(e) => {
                        walk_contained_exprs(e, visit, through_arrows)
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        ExprKind::Object(object) => {
            for prop in &object.props {
                match prop {
                    ObjectProperty::Init { key, value, .. } => {
                        walk_contained_property_name(key, visit, through_arrows);
                        walk_contained_exprs(value, visit, through_arrows);
                    }
                    ObjectProperty::Method { key, .. }
                    | ObjectProperty::Get { key, .. }
                    | ObjectProperty::Set { key, .. } => {
                        walk_contained_property_name(key, visit, through_arrows)
                    }
                    ObjectProperty::Spread(e) => walk_contained_exprs(e, visit, through_arrows),
                }
            }
        }
        ExprKind::Unary { operand, .. } => walk_contained_exprs(operand, visit, through_arrows),
        ExprKind::Update { target, .. } => walk_contained_exprs(target, visit, through_arrows),
        ExprKind::Binary { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            walk_contained_exprs(left, visit, through_arrows);
            walk_contained_exprs(right, visit, through_arrows);
        }
        ExprKind::Assign { target, value, .. } => {
            walk_contained_exprs(target, visit, through_arrows);
            walk_contained_exprs(value, visit, through_arrows);
        }
        ExprKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            walk_contained_exprs(test, visit, through_arrows);
            walk_contained_exprs(consequent, visit, through_arrows);
            walk_contained_exprs(alternate, visit, through_arrows);
        }
        ExprKind::Call(call) => {
            walk_contained_exprs(&call.callee, visit, through_arrows);
            for arg in &call.args {
                walk_contained_argument(arg, visit, through_arrows);
            }
        }
        ExprKind::New(new) => {
            walk_contained_exprs(&new.callee, visit, through_arrows);
            for arg in &new.args {
                walk_contained_argument(arg, visit, through_arrows);
            }
        }
        ExprKind::Member(member) => {
            walk_contained_exprs(&member.object, visit, through_arrows);
            if let syntax::MemberProperty::Computed(index) = &member.property {
                walk_contained_exprs(index, visit, through_arrows);
            }
        }
        ExprKind::TaggedTemplate { tag, quasi } => {
            walk_contained_exprs(tag, visit, through_arrows);
            for e in &quasi.exprs {
                walk_contained_exprs(e, visit, through_arrows);
            }
        }
        ExprKind::Template(template) => {
            for e in &template.exprs {
                walk_contained_exprs(e, visit, through_arrows);
            }
        }
        ExprKind::Paren(inner) => walk_contained_exprs(inner, visit, through_arrows),
        ExprKind::Sequence(exprs) => {
            for e in exprs {
                walk_contained_exprs(e, visit, through_arrows);
            }
        }
        ExprKind::Yield { argument, .. } => {
            if let Some(argument) = argument {
                walk_contained_exprs(argument, visit, through_arrows);
            }
        }
        ExprKind::Await(operand) => walk_contained_exprs(operand, visit, through_arrows),
        ExprKind::ImportCall {
            specifier, options, ..
        } => {
            walk_contained_exprs(specifier, visit, through_arrows);
            if let Some(options) = options {
                walk_contained_exprs(options, visit, through_arrows);
            }
        }
    }
}

fn walk_contained_argument(
    argument: &Argument,
    visit: &mut impl FnMut(&Expr),
    through_arrows: bool,
) {
    match argument {
        Argument::Expr(e) | Argument::Spread(e) => walk_contained_exprs(e, visit, through_arrows),
    }
}

fn walk_contained_property_name(
    name: &PropertyName,
    visit: &mut impl FnMut(&Expr),
    through_arrows: bool,
) {
    if let PropertyName::Computed(expr) = name {
        walk_contained_exprs(expr, visit, through_arrows);
    }
}

/// The parts of a nested class that are evaluated in the enclosing class's
/// containment: the heritage, computed element names, and field
/// initializers. Method bodies/params and static blocks have their own
/// `arguments`/`await` rules.
fn walk_contained_class(class: &Class, visit: &mut impl FnMut(&Expr), through_arrows: bool) {
    if let Some(heritage) = &class.heritage {
        walk_contained_exprs(heritage, visit, through_arrows);
    }
    for element in &class.elements {
        match element {
            ClassElement::Method { name, .. }
            | ClassElement::Get { name, .. }
            | ClassElement::Set { name, .. } => {
                walk_contained_class_name(name, visit, through_arrows)
            }
            ClassElement::Field { name, init, .. } => {
                walk_contained_class_name(name, visit, through_arrows);
                if let Some(init) = init {
                    walk_contained_exprs(init, visit, through_arrows);
                }
            }
            ClassElement::StaticBlock(_) => {}
        }
    }
}

fn walk_contained_class_name(
    name: &ClassElementName,
    visit: &mut impl FnMut(&Expr),
    through_arrows: bool,
) {
    if let ClassElementName::Property(PropertyName::Computed(expr)) = name {
        walk_contained_exprs(expr, visit, through_arrows);
    }
}

/// Statement-list counterpart of `walk_contained_exprs`.
fn walk_contained_stmts(stmts: &[Stmt], visit: &mut impl FnMut(&Expr), through_arrows: bool) {
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::Block(block) => walk_contained_stmts(&block.stmts, visit, through_arrows),
            StmtKind::Expr(expr) => walk_contained_exprs(expr, visit, through_arrows),
            StmtKind::If {
                test,
                consequent,
                alternate,
            } => {
                walk_contained_exprs(test, visit, through_arrows);
                walk_contained_stmts(std::slice::from_ref(consequent), visit, through_arrows);
                if let Some(alt) = alternate {
                    walk_contained_stmts(std::slice::from_ref(alt), visit, through_arrows);
                }
            }
            StmtKind::VarDecl { decls, .. } | StmtKind::UsingDecl { decls, .. } => {
                for decl in decls {
                    if let Some(init) = &decl.init {
                        walk_contained_exprs(init, visit, through_arrows);
                    }
                }
            }
            StmtKind::Return(Some(expr)) | StmtKind::Throw(expr) => {
                walk_contained_exprs(expr, visit, through_arrows)
            }
            StmtKind::Return(None) => {}
            StmtKind::While { test, body } => {
                walk_contained_exprs(test, visit, through_arrows);
                walk_contained_stmts(std::slice::from_ref(body), visit, through_arrows);
            }
            StmtKind::DoWhile { body, test } => {
                walk_contained_stmts(std::slice::from_ref(body), visit, through_arrows);
                walk_contained_exprs(test, visit, through_arrows);
            }
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => {
                walk_contained_for_init(init, visit, through_arrows);
                if let Some(t) = test {
                    walk_contained_exprs(t, visit, through_arrows);
                }
                if let Some(u) = update {
                    walk_contained_exprs(u, visit, through_arrows);
                }
                walk_contained_stmts(std::slice::from_ref(body), visit, through_arrows);
            }
            StmtKind::ForIn { left, right, body }
            | StmtKind::ForOf {
                left, right, body, ..
            } => {
                walk_contained_for_binding(left, visit, through_arrows);
                walk_contained_exprs(right, visit, through_arrows);
                walk_contained_stmts(std::slice::from_ref(body), visit, through_arrows);
            }
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => {
                walk_contained_stmts(&block.stmts, visit, through_arrows);
                if let Some(handler) = handler {
                    walk_contained_stmts(&handler.body.stmts, visit, through_arrows);
                }
                if let Some(finalizer) = finalizer {
                    walk_contained_stmts(&finalizer.stmts, visit, through_arrows);
                }
            }
            StmtKind::Switch {
                discriminant,
                cases,
            } => {
                walk_contained_exprs(discriminant, visit, through_arrows);
                for case in cases {
                    if let Some(test) = &case.test {
                        walk_contained_exprs(test, visit, through_arrows);
                    }
                    walk_contained_stmts(&case.consequent, visit, through_arrows);
                }
            }
            StmtKind::With { object, body } => {
                walk_contained_exprs(object, visit, through_arrows);
                walk_contained_stmts(std::slice::from_ref(body), visit, through_arrows);
            }
            StmtKind::Labeled { body, .. } => {
                walk_contained_stmts(std::slice::from_ref(body), visit, through_arrows)
            }
            StmtKind::FunctionDecl(_) => {}
            StmtKind::ClassDecl(class) => walk_contained_class(class, visit, through_arrows),
            StmtKind::Empty | StmtKind::Debugger | StmtKind::Break(_) | StmtKind::Continue(_) => {}
        }
    }
}

fn walk_contained_for_init(
    init: &Option<syntax::ForInit>,
    visit: &mut impl FnMut(&Expr),
    through_arrows: bool,
) {
    match init {
        Some(syntax::ForInit::Expr(expr)) => walk_contained_exprs(expr, visit, through_arrows),
        Some(syntax::ForInit::VarDecl { decls, .. }) => {
            for decl in decls {
                if let Some(init) = &decl.init {
                    walk_contained_exprs(init, visit, through_arrows);
                }
            }
        }
        None => {}
    }
}

fn walk_contained_for_binding(
    binding: &syntax::ForBinding,
    visit: &mut impl FnMut(&Expr),
    through_arrows: bool,
) {
    match binding {
        syntax::ForBinding::Expr(expr) => walk_contained_exprs(expr, visit, through_arrows),
        syntax::ForBinding::VarDecl { init, .. } => {
            if let Some(init) = init {
                walk_contained_exprs(init, visit, through_arrows);
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
        ExportName::Str(value) => {
            // A string export name must be well-formed UTF-16: an unpaired
            // surrogate in an ExportedName is a Syntax Error (spec 16.2.3).
            if !is_well_formed_utf16(value.as_slice()) {
                return Err(error_at(span, "Export name contains an unpaired surrogate"));
            }
            register_export_string(seen, value, span)
        }
    }
}

fn is_well_formed_utf16(units: &[u16]) -> bool {
    let mut iter = units.iter();
    while let Some(&unit) = iter.next() {
        if (0xD800..=0xDBFF).contains(&unit) {
            match iter.next() {
                Some(&low) if (0xDC00..=0xDFFF).contains(&low) => {}
                _ => return false,
            }
        } else if (0xDC00..=0xDFFF).contains(&unit) {
            return false;
        }
    }
    true
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
    check_expr_with(expr, labels, false)
}

/// Like `check_expr`, but descending into a destructuring-assignment target:
/// the `__proto__` duplicate rule applies to ObjectInitializers, not to
/// ObjectAssignmentPatterns (spec 13.2.5, Annex B.2.2).
fn check_expr_with(expr: &Expr, labels: &mut LabelState, pattern: bool) -> Result<(), JsError> {
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
        ExprKind::Object(object) => check_object_literal(object, labels, pattern),
        ExprKind::Array(array) => {
            for element in &array.elements {
                match element {
                    ArrayElement::Expr(e) | ArrayElement::Spread(e) => {
                        check_expr_with(e, labels, pattern)?;
                    }
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
            check_expr_with(target, labels, true)?;
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
        ExprKind::ImportCall {
            specifier, options, ..
        } => {
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

/// `__proto__` may appear as a data property only once in an object
/// initializer (spec 13.2.5 early errors); shorthand and method entries do
/// not count, and assignment-pattern objects are exempt.
fn check_object_literal(
    object: &ObjectLiteral,
    labels: &mut LabelState,
    pattern: bool,
) -> Result<(), JsError> {
    if !pattern {
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
    }
    for prop in &object.props {
        match prop {
            ObjectProperty::Init { key, value, .. } => {
                if let PropertyName::Computed(computed) = key {
                    check_expr(computed, labels)?;
                }
                check_expr_with(value, labels, pattern)?;
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

// ---- AllPrivateNamesValid (spec 15.7.9) ----

/// Validates every private-name reference: the name must be declared in the
/// class containing the reference or in an enclosing class (the private
/// environment is lexically scoped, and forward references within a class
/// are allowed). A class's heritage is evaluated with the class's own
/// private names not yet in scope, so those are excluded there.
pub(crate) fn check_private_names(program: &Program) -> Result<(), JsError> {
    let mut env = Vec::new();
    check_private_stmts(&program.body, &mut env)
}

/// Like `check_private_names`, for a Module's items.
pub(crate) fn check_private_names_module(module: &Module) -> Result<(), JsError> {
    let mut env = Vec::new();
    for item in &module.body {
        match item {
            ModuleItem::Stmt(stmt) => check_private_stmt(stmt, &mut env)?,
            ModuleItem::Import(_) => {}
            ModuleItem::Export(decl) => match decl {
                ExportDecl::Named { .. } | ExportDecl::From { .. } => {}
                ExportDecl::Declaration(stmt) => check_private_stmt(stmt, &mut env)?,
                ExportDecl::Default(inner) => match &**inner {
                    syntax::ExportDefault::Function(f) => check_private_function(f, &mut env)?,
                    syntax::ExportDefault::Class(c) => check_private_class(c, &mut env)?,
                    syntax::ExportDefault::Expr(e) => check_private_expr(e, &mut env)?,
                },
            },
        }
    }
    Ok(())
}

fn private_names_contain(env: &[std::collections::HashSet<AtomId>], name: AtomId) -> bool {
    env.iter().rev().any(|set| set.contains(&name))
}

fn check_private_class(
    class: &Class,
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    let names: std::collections::HashSet<AtomId> = class
        .elements
        .iter()
        .filter_map(|element| match element {
            ClassElement::Method { name, .. }
            | ClassElement::Get { name, .. }
            | ClassElement::Set { name, .. }
            | ClassElement::Field { name, .. } => match name {
                ClassElementName::Private(atom) => Some(*atom),
                ClassElementName::Property(_) => None,
            },
            ClassElement::StaticBlock(_) => None,
        })
        .collect();
    if let Some(heritage) = &class.heritage {
        check_private_expr(heritage, env)?;
    }
    env.push(names);
    for element in &class.elements {
        check_private_element(element, env)?;
    }
    env.pop();
    Ok(())
}

fn check_private_element(
    element: &ClassElement,
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    match element {
        ClassElement::Method { name, function, .. } => {
            check_private_class_name(name, env)?;
            check_private_function(function, env)
        }
        ClassElement::Get { name, body, .. } => {
            check_private_class_name(name, env)?;
            check_private_stmts(&body.stmts, env)
        }
        ClassElement::Set {
            name,
            param,
            init,
            body,
            ..
        } => {
            check_private_class_name(name, env)?;
            check_private_binding_pattern(param, env)?;
            if let Some(init) = init {
                check_private_expr(init, env)?;
            }
            check_private_stmts(&body.stmts, env)
        }
        ClassElement::Field { name, init, .. } => {
            check_private_class_name(name, env)?;
            if let Some(init) = init {
                check_private_expr(init, env)?;
            }
            Ok(())
        }
        ClassElement::StaticBlock(block) => check_private_stmts(&block.stmts, env),
    }
}

fn check_private_class_name(
    name: &ClassElementName,
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    if let ClassElementName::Property(PropertyName::Computed(expr)) = name {
        check_private_expr(expr, env)?;
    }
    Ok(())
}

fn check_private_function(
    function: &Function,
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    for param in &function.params {
        check_private_binding_element(param, env)?;
    }
    check_private_stmts(&function.body.stmts, env)
}

fn check_private_binding_element(
    element: &BindingElement,
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    check_private_binding_pattern(&element.pattern, env)?;
    if let Some(init) = &element.init {
        check_private_expr(init, env)?;
    }
    Ok(())
}

fn check_private_binding_pattern(
    pattern: &BindingPattern,
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    match pattern {
        BindingPattern::Ident(_) => Ok(()),
        BindingPattern::Object(props) => {
            for prop in props {
                match prop {
                    ObjectBindingProperty::Property { element, .. }
                    | ObjectBindingProperty::Rest(element) => {
                        check_private_binding_element(element, env)?;
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
                        check_private_binding_element(e, env)?;
                    }
                }
            }
            Ok(())
        }
    }
}

fn check_private_stmts(
    stmts: &[Stmt],
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    for stmt in stmts {
        check_private_stmt(stmt, env)?;
    }
    Ok(())
}

fn check_private_stmt(
    stmt: &Stmt,
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    match &stmt.kind {
        StmtKind::Block(block) => check_private_stmts(&block.stmts, env),
        StmtKind::Empty | StmtKind::Debugger => Ok(()),
        StmtKind::Expr(expr) => check_private_expr(expr, env),
        StmtKind::If {
            test,
            consequent,
            alternate,
        } => {
            check_private_expr(test, env)?;
            check_private_stmt(consequent, env)?;
            if let Some(alt) = alternate {
                check_private_stmt(alt, env)?;
            }
            Ok(())
        }
        StmtKind::VarDecl { decls, .. } | StmtKind::UsingDecl { decls, .. } => {
            for decl in decls {
                check_private_binding_pattern(&decl.pattern, env)?;
                if let Some(init) = &decl.init {
                    check_private_expr(init, env)?;
                }
            }
            Ok(())
        }
        StmtKind::FunctionDecl(f) => check_private_function(f, env),
        StmtKind::ClassDecl(c) => check_private_class(c, env),
        StmtKind::Return(Some(expr)) | StmtKind::Throw(expr) => check_private_expr(expr, env),
        StmtKind::Return(None) => Ok(()),
        StmtKind::Labeled { body, .. } => check_private_stmt(body, env),
        StmtKind::Break(_) | StmtKind::Continue(_) => Ok(()),
        StmtKind::While { test, body } => {
            check_private_expr(test, env)?;
            check_private_stmt(body, env)
        }
        StmtKind::DoWhile { body, test } => {
            check_private_stmt(body, env)?;
            check_private_expr(test, env)
        }
        StmtKind::For {
            init,
            test,
            update,
            body,
        } => {
            check_private_for_init(init, env)?;
            if let Some(t) = test {
                check_private_expr(t, env)?;
            }
            if let Some(u) = update {
                check_private_expr(u, env)?;
            }
            check_private_stmt(body, env)
        }
        StmtKind::ForIn { left, right, body } => {
            check_private_for_binding(left, env)?;
            check_private_expr(right, env)?;
            check_private_stmt(body, env)
        }
        StmtKind::ForOf {
            left, right, body, ..
        } => {
            check_private_for_binding(left, env)?;
            check_private_expr(right, env)?;
            check_private_stmt(body, env)
        }
        StmtKind::Try {
            block,
            handler,
            finalizer,
        } => {
            check_private_stmts(&block.stmts, env)?;
            if let Some(handler) = handler {
                if let Some(param) = &handler.param {
                    check_private_binding_pattern(param, env)?;
                }
                check_private_stmts(&handler.body.stmts, env)?;
            }
            if let Some(finalizer) = finalizer {
                check_private_stmts(&finalizer.stmts, env)?;
            }
            Ok(())
        }
        StmtKind::Switch {
            discriminant,
            cases,
        } => {
            check_private_expr(discriminant, env)?;
            for case in cases {
                if let Some(test) = &case.test {
                    check_private_expr(test, env)?;
                }
                check_private_stmts(&case.consequent, env)?;
            }
            Ok(())
        }
        StmtKind::With { object, body } => {
            check_private_expr(object, env)?;
            check_private_stmt(body, env)
        }
    }
}

fn check_private_for_init(
    init: &Option<syntax::ForInit>,
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    match init {
        Some(syntax::ForInit::Expr(expr)) => check_private_expr(expr, env),
        Some(syntax::ForInit::VarDecl { decls, .. }) => {
            for decl in decls {
                check_private_binding_pattern(&decl.pattern, env)?;
                if let Some(init) = &decl.init {
                    check_private_expr(init, env)?;
                }
            }
            Ok(())
        }
        None => Ok(()),
    }
}

fn check_private_for_binding(
    binding: &syntax::ForBinding,
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    match binding {
        syntax::ForBinding::Expr(expr) => check_private_expr(expr, env),
        syntax::ForBinding::VarDecl { pattern, init, .. } => {
            check_private_binding_pattern(pattern, env)?;
            if let Some(init) = init {
                check_private_expr(init, env)?;
            }
            Ok(())
        }
    }
}

fn check_private_expr(
    expr: &Expr,
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    match &expr.kind {
        ExprKind::Literal(_)
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::MetaProperty { .. } => Ok(()),
        ExprKind::Function(f) => check_private_function(f, env),
        ExprKind::Class(c) => check_private_class(c, env),
        ExprKind::Arrow { params, body, .. } => {
            for param in params {
                check_private_binding_element(param, env)?;
            }
            match body {
                ArrowBody::Expr(expr) => check_private_expr(expr, env),
                ArrowBody::Block(block) => check_private_stmts(&block.stmts, env),
            }
        }
        ExprKind::PrivateIn { name, object } => {
            if !private_names_contain(env, *name) {
                return Err(error_at(
                    expr.span,
                    "Private field must be declared in an enclosing class",
                ));
            }
            check_private_expr(object, env)
        }
        ExprKind::Array(array) => {
            for element in &array.elements {
                match element {
                    ArrayElement::Expr(e) | ArrayElement::Spread(e) => check_private_expr(e, env)?,
                    ArrayElement::Hole => {}
                }
            }
            Ok(())
        }
        ExprKind::Object(object) => {
            for prop in &object.props {
                match prop {
                    ObjectProperty::Init { key, value, .. } => {
                        check_private_property_name(key, env)?;
                        check_private_expr(value, env)?;
                    }
                    ObjectProperty::Method { key, function } => {
                        check_private_property_name(key, env)?;
                        check_private_function(function, env)?;
                    }
                    ObjectProperty::Get { key, body } | ObjectProperty::Set { key, body, .. } => {
                        check_private_property_name(key, env)?;
                        check_private_stmts(&body.stmts, env)?;
                    }
                    ObjectProperty::Spread(e) => check_private_expr(e, env)?,
                }
            }
            Ok(())
        }
        ExprKind::Unary { operand, .. } => check_private_expr(operand, env),
        ExprKind::Update { target, .. } => check_private_expr(target, env),
        ExprKind::Binary { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            check_private_expr(left, env)?;
            check_private_expr(right, env)
        }
        ExprKind::Assign { target, value, .. } => {
            check_private_expr(target, env)?;
            check_private_expr(value, env)
        }
        ExprKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            check_private_expr(test, env)?;
            check_private_expr(consequent, env)?;
            check_private_expr(alternate, env)
        }
        ExprKind::Call(call) => {
            check_private_expr(&call.callee, env)?;
            for arg in &call.args {
                match arg {
                    Argument::Expr(e) | Argument::Spread(e) => check_private_expr(e, env)?,
                }
            }
            Ok(())
        }
        ExprKind::New(new) => {
            check_private_expr(&new.callee, env)?;
            for arg in &new.args {
                match arg {
                    Argument::Expr(e) | Argument::Spread(e) => check_private_expr(e, env)?,
                }
            }
            Ok(())
        }
        ExprKind::Member(member) => {
            if let syntax::MemberProperty::Private(name) = member.property
                && !private_names_contain(env, name)
            {
                return Err(error_at(
                    member.span,
                    "Private field must be declared in an enclosing class",
                ));
            }
            check_private_expr(&member.object, env)?;
            if let syntax::MemberProperty::Computed(index) = &member.property {
                check_private_expr(index, env)?;
            }
            Ok(())
        }
        ExprKind::TaggedTemplate { tag, quasi } => {
            check_private_expr(tag, env)?;
            for e in &quasi.exprs {
                check_private_expr(e, env)?;
            }
            Ok(())
        }
        ExprKind::Template(template) => {
            for e in &template.exprs {
                check_private_expr(e, env)?;
            }
            Ok(())
        }
        ExprKind::Paren(inner) => check_private_expr(inner, env),
        ExprKind::Sequence(exprs) => {
            for e in exprs {
                check_private_expr(e, env)?;
            }
            Ok(())
        }
        ExprKind::Yield { argument, .. } => {
            if let Some(argument) = argument {
                check_private_expr(argument, env)?;
            }
            Ok(())
        }
        ExprKind::Await(operand) => check_private_expr(operand, env),
        ExprKind::ImportCall {
            specifier, options, ..
        } => {
            check_private_expr(specifier, env)?;
            if let Some(options) = options {
                check_private_expr(options, env)?;
            }
            Ok(())
        }
    }
}

fn check_private_property_name(
    name: &PropertyName,
    env: &mut Vec<std::collections::HashSet<AtomId>>,
) -> Result<(), JsError> {
    if let PropertyName::Computed(expr) = name {
        check_private_expr(expr, env)?;
    }
    Ok(())
}
