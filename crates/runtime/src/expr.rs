//! Expression evaluation (spec ch. 13).
//!
//! Phase 6 evaluates every expression form: literals, identifiers, `this`,
//! array/object literals (with holes, spread, and `__proto__`), member
//! access and calls (with optional chaining), `new`, unary/update/binary/
//! logical/conditional/assignment/comma operators, and template literals.
//! Function/class/arrow expressions, destructuring, and `yield`/`await`
//! join with Phase 7.

use crux::bigint;
use crux::convert::{ToPrimitiveHint, to_boolean, to_number, to_numeric, to_string};
use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::ops::{is_strictly_equal, same_value};
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_callable, type_of};
use syntax::ast::{
    Argument, ArrayElement, AssignOp, BinaryOp, BindingElement, Expr, ExprKind, Literal, LogicalOp,
    MemberExpr, MemberProperty, ObjectLiteral, ObjectProperty, PropertyName, TemplateLiteral,
    UnaryOp, UpdateOp,
};

use crate::agent::Agent;
use crate::context::{
    Reference, ReferenceBase, delete_property_or_throw, get_property, get_property_key,
    get_this_value, get_value, put_value, resolve_binding,
};

/// Evaluate an expression to a value (spec 13.1.1).
pub fn eval_expr(agent: &mut Agent, expr: &Expr, strict: bool) -> Result<Value, JsError> {
    match &expr.kind {
        ExprKind::Literal(literal) => eval_literal(agent, literal),
        ExprKind::Ident(name) => {
            let name = crux::lookup(*name);
            let reference = resolve_binding(agent, &name, strict)?;
            get_value(agent, &reference)
        }
        ExprKind::This => crate::context::resolve_this_binding(agent),
        ExprKind::Super => Err(JsError::new(
            ErrorKind::ReferenceError,
            "super is not valid here".into(),
        )),
        ExprKind::Array(literal) => eval_array_literal(agent, literal, strict),
        ExprKind::Object(literal) => eval_object_literal(agent, literal, strict),
        ExprKind::Function(f) => {
            let env = agent.running_context()?.lexical_environment.clone();
            crate::function::instantiate_function_expression(
                agent,
                f,
                env,
                strict,
                Vec::new(),
                Vec::new(),
            )
        }
        ExprKind::Arrow {
            is_async,
            params,
            body,
        } => {
            let env = agent.running_context()?.lexical_environment.clone();
            crate::function::instantiate_arrow(
                agent,
                *is_async,
                params.clone(),
                body.clone(),
                env,
                strict,
                Vec::new(),
                Vec::new(),
            )
        }
        ExprKind::Class(class) => {
            crate::class::class_definition_evaluation(agent, class, class.name, strict)
        }
        ExprKind::Unary { op, operand } => eval_unary(agent, op, operand, strict),
        ExprKind::Update { op, prefix, target } => eval_update(agent, op, *prefix, target, strict),
        ExprKind::Binary { op, left, right } => {
            let left = eval_expr(agent, left, strict)?;
            let right = eval_expr(agent, right, strict)?;
            apply_binary(agent, *op, &left, &right)
        }
        ExprKind::Logical { op, left, right } => eval_logical(agent, *op, left, right, strict),
        ExprKind::Assign { op, target, value } => eval_assignment(agent, op, target, value, strict),
        ExprKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            let test = eval_expr(agent, test, strict)?;
            if to_boolean(&test) {
                eval_expr(agent, consequent, strict)
            } else {
                eval_expr(agent, alternate, strict)
            }
        }
        ExprKind::PrivateIn { name, object } => {
            // PrivateIn (spec 13.11.1): the `#name in obj` brand check. The
            // right-hand side must be an object (spec 13.10.3).
            let name_id = crate::context::resolve_private_name(agent, *name)?.id;
            let object = eval_expr(agent, object, strict)?;
            if !matches!(object.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Cannot use 'in' operator with a non-object value".into(),
                ));
            }
            Ok(Value::Boolean(crate::context::private_in(
                &object, name_id,
            )?))
        }
        ExprKind::Call(call) => eval_call(agent, call, strict),
        ExprKind::New(new) => eval_new(agent, new, strict),
        ExprKind::Member(_) => {
            let reference = eval_chain(agent, expr, strict)?;
            match reference {
                None => Ok(Value::Undefined),
                Some(ChainResult::Reference(reference)) => get_reference_value(agent, &reference),
                Some(ChainResult::Value(value)) => Ok(value),
            }
        }
        ExprKind::TaggedTemplate { tag, quasi } => eval_tagged_template(agent, tag, quasi, strict),
        ExprKind::Template(template) => eval_template(agent, template, strict),
        ExprKind::Paren(inner) => eval_expr(agent, inner, strict),
        ExprKind::Sequence(exprs) => {
            let mut value = Value::Undefined;
            for expr in exprs {
                value = eval_expr(agent, expr, strict)?;
            }
            Ok(value)
        }
        ExprKind::Yield { .. } | ExprKind::Await(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "yield and await are not implemented until Phase 7".into(),
        )),
        ExprKind::MetaProperty { meta, .. } => {
            let meta = crux::lookup(*meta);
            if meta.as_slice() == "new".encode_utf16().collect::<Vec<_>>().as_slice() {
                // new.target (spec 13.3.5.3): the active constructor, or
                // *undefined* at the script level.
                crate::context::get_new_target(agent)
            } else {
                // import.meta (spec 13.3.7): a fresh ordinary object per
                // module evaluation.
                crate::module::import_meta(agent)
            }
        }
        ExprKind::ImportCall {
            specifier,
            options,
            phase,
        } => {
            let specifier = eval_expr(agent, specifier, strict)?;
            let options = match options {
                Some(expr) => Some(eval_expr(agent, expr, strict)?),
                None => None,
            };
            crate::module::dynamic_import(agent, &specifier, options.as_ref(), *phase)
        }
    }
}

/// Whether the operand (through parentheses) is a `super.x`/`super[key]`
/// member expression.
fn is_super_member_operand(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Paren(inner) => is_super_member_operand(inner),
        ExprKind::Member(member) => matches!(member.object.kind, ExprKind::Super),
        _ => false,
    }
}

/// The result of evaluating an optional-chain link: a Reference (member or
/// identifier, keeping the `this` for calls) or a plain value.
pub enum ChainResult {
    Reference(Reference),
    Value(Value),
}

/// Evaluate an optional chain: member links, call links, and the base
/// expression. `None` means a `?.` link short-circuited (the base was
/// nullish) and the rest of the chain must not be evaluated.
pub fn eval_chain(
    agent: &mut Agent,
    expr: &Expr,
    strict: bool,
) -> Result<Option<ChainResult>, JsError> {
    match &expr.kind {
        // Parentheses do not change the reference-ness of the wrapped
        // expression: `typeof (x)` on an unresolvable `x` is still
        // "undefined", and `delete (obj.prop)` still deletes (spec 13.2.8).
        ExprKind::Paren(inner) => eval_chain(agent, inner, strict),
        ExprKind::Member(member) => {
            if matches!(member.object.kind, ExprKind::Super) {
                // SuperProperty (spec 13.3.7.1): GetThisBinding runs before
                // the key expression is evaluated, so `super[super()]` in a
                // derived constructor throws a ReferenceError for the
                // uninitialized `this` instead of running the inner call.
                let this = crate::context::resolve_this_binding(agent)?;
                let base = crate::context::get_super_base(agent)?;
                let name = eval_member_key(agent, member, strict)?;
                let reference = Reference {
                    base: ReferenceBase::Value(base),
                    name,
                    strict,
                    this_value: Some(this),
                    private_name: None,
                };
                return Ok(Some(ChainResult::Reference(reference)));
            }
            if let MemberProperty::Private(atom) = &member.property {
                // `object.#name` — a private member reference (PrivateGet /
                // PrivateSet at access time), resolved in the running
                // PrivateEnvironment.
                let name_id = crate::context::resolve_private_name(agent, *atom)?.id;
                let object = eval_chain(agent, &member.object, strict)?;
                let Some(object) = object else {
                    return Ok(None);
                };
                let object_value = match object {
                    ChainResult::Reference(reference) => get_reference_value(agent, &reference)?,
                    ChainResult::Value(value) => value,
                };
                if member.optional && is_nullish(&object_value) {
                    return Ok(None);
                }
                let reference = Reference {
                    base: ReferenceBase::Value(object_value),
                    name: PropertyKey::from_utf8(""),
                    strict,
                    this_value: None,
                    private_name: Some(name_id),
                };
                return Ok(Some(ChainResult::Reference(reference)));
            }
            let object = eval_chain(agent, &member.object, strict)?;
            let Some(object) = object else {
                return Ok(None);
            };
            let object_value = match object {
                ChainResult::Reference(reference) => get_reference_value(agent, &reference)?,
                ChainResult::Value(value) => value,
            };
            // A computed property key is evaluated before the nullish-base
            // check (spec 13.3.2.2: RequireObjectCoercible precedes
            // ToPropertyKey), so `null[key]` runs the key expression but
            // throws TypeError before converting it. An optional `?.[` link
            // short-circuits on a nullish base without evaluating its key.
            let computed_key = match &member.property {
                MemberProperty::Computed(expr) if !member.optional => {
                    Some(eval_expr(agent, expr, strict)?)
                }
                _ => None,
            };
            if is_nullish(&object_value) {
                if member.optional {
                    return Ok(None);
                }
                return Err(nullish_member_error(member));
            }
            let name = match computed_key {
                Some(key) => crate::context::to_property_key(agent, &key)?,
                None => eval_member_key(agent, member, strict)?,
            };
            let reference = Reference {
                base: ReferenceBase::Value(object_value),
                name,
                strict,
                this_value: None,
                private_name: None,
            };
            Ok(Some(ChainResult::Reference(reference)))
        }
        ExprKind::Call(call) => eval_call_chain(agent, call, strict),
        ExprKind::Ident(name) => {
            let name = crux::lookup(*name);
            Ok(Some(ChainResult::Reference(resolve_binding(
                agent, &name, strict,
            )?)))
        }
        _ => Ok(Some(ChainResult::Value(eval_expr(agent, expr, strict)?))),
    }
}

/// Evaluate the callee chain and perform the call, honoring `?.` on both the
/// callee link and the call link.
fn eval_call_chain(
    agent: &mut Agent,
    call: &syntax::ast::CallExpr,
    strict: bool,
) -> Result<Option<ChainResult>, JsError> {
    if matches!(call.callee.kind, ExprKind::Super) {
        // SuperCall (spec 13.3.5.1): construct the superclass with the
        // current newTarget, bind the result as `this`, and initialize the
        // derived class's instance fields.
        let new_target = crate::context::get_new_target(agent)?;
        let super_ctor = crate::context::get_super_constructor(agent)?;
        let args = eval_arguments(agent, &call.args, strict)?;
        let result = crate::function::construct(agent, &super_ctor, &args, &new_target)?;
        let this_env = crate::context::get_this_environment(agent)?;
        this_env.bind_this_value(result.clone())?;
        if let Some(function_value) = agent.running_context()?.function.clone() {
            crate::function::initialize_instance_elements(agent, &result, &function_value)?;
        }
        return Ok(Some(ChainResult::Value(result)));
    }
    let callee = eval_chain(agent, &call.callee, strict)?;
    let Some(callee) = callee else {
        return Ok(None);
    };
    let (this, callee_value) = match callee {
        ChainResult::Reference(reference) => {
            let this = get_this_value(&reference);
            (this, get_reference_value(agent, &reference)?)
        }
        ChainResult::Value(value) => (Value::Undefined, value),
    };
    if call.optional && is_nullish(&callee_value) {
        return Ok(None);
    }
    // spec 13.3.6.1: the arguments are evaluated before the callable check
    // (step 5 precedes step 6), so `o.bar(foo())` runs `foo()` even though
    // `o.bar` is not callable.
    let args = eval_arguments(agent, &call.args, strict)?;
    if !is_callable(&callee_value) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("{} is not a function", type_of(&callee_value)),
        ));
    }
    // Direct eval (spec 13.3.6.1 step 5): a call whose callee is the
    // intrinsic %eval% reached through the identifier `eval` runs its first
    // argument as a Script; any other route to %eval% is an indirect eval.
    if is_eval_function(agent, &callee_value)? {
        // spec 19.2.1.1 step 2: a non-string argument (including a missing
        // one, which is *undefined*) is returned as-is — eval never coerces
        // (S15.1.2.1_A1.1_T2).
        let source = args.first().cloned().unwrap_or(Value::Undefined);
        let ValueKind::String(source) = source.kind() else {
            return Ok(Some(ChainResult::Value(source)));
        };
        // Only a plain identifier callee makes a direct eval; an optional
        // call `eval?.(...)` routes the callee through the chain and is an
        // indirect eval (spec 13.3.9.1: the callee is a value, not the
        // `eval` identifier reference).
        let direct = !call.optional
            && matches!(
                call.callee.kind,
                ExprKind::Ident(id) if crux::lookup(id) == crux::string::JsString::from_utf8("eval")
            );
        let result = crate::script::perform_eval(agent, source.as_ref(), strict, direct)?;
        return Ok(Some(ChainResult::Value(result)));
    }
    let result = crate::function::call(agent, &callee_value, this, &args)?;
    Ok(Some(ChainResult::Value(result)))
}

/// Whether the value is the current realm's `%eval%` intrinsic.
fn is_eval_function(agent: &Agent, value: &Value) -> Result<bool, JsError> {
    let realm = agent.current_realm()?;
    Ok(realm.intrinsics.get("%eval%").as_ref() == Some(value))
}

fn nullish_member_error(member: &MemberExpr) -> JsError {
    let name = match &member.property {
        MemberProperty::Name(id) => crux::lookup(*id).to_string_lossy(),
        MemberProperty::Computed(_) => "<computed>".into(),
        MemberProperty::Private(id) => crux::lookup(*id).to_string_lossy(),
    };
    JsError::new(
        ErrorKind::TypeError,
        format!("Cannot read properties of null (reading {name:?})"),
    )
}

fn is_nullish(value: &Value) -> bool {
    matches!(value.kind(), ValueKind::Undefined | ValueKind::Null)
}

/// The referenced property key of a member access.
fn eval_member_key(
    agent: &mut Agent,
    member: &MemberExpr,
    strict: bool,
) -> Result<PropertyKey, JsError> {
    match &member.property {
        MemberProperty::Name(id) => Ok(PropertyKey::String(*id)),
        MemberProperty::Private(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "private member access is not implemented until Phase 7".into(),
        )),
        MemberProperty::Computed(expr) => {
            let key = eval_expr(agent, expr, strict)?;
            crate::context::to_property_key(agent, &key)
        }
    }
}

/// Evaluate a LeftHandSideExpression to a Reference (spec 13.3.1), for
/// assignment targets and `delete`.
pub fn eval_reference(agent: &mut Agent, expr: &Expr, strict: bool) -> Result<Reference, JsError> {
    if let ExprKind::Paren(inner) = &expr.kind {
        return eval_reference(agent, inner, strict);
    }
    let chain = eval_chain(agent, expr, strict)?;
    match chain {
        Some(ChainResult::Reference(reference)) => Ok(reference),
        Some(ChainResult::Value(_)) | None => Err(JsError::new(
            ErrorKind::ReferenceError,
            "Invalid left-hand side in assignment".into(),
        )),
    }
}

/// GetValue of a Reference, honoring a super reference's [[ThisValue]]
/// receiver: the property read is base.[[Get]](name, GetThisValue(V)) (spec
/// 6.2.3.1 step 5.b), not base.[[Get]](name, base).
fn get_reference_value(agent: &mut Agent, reference: &Reference) -> Result<Value, JsError> {
    let Some(receiver) = &reference.this_value else {
        return get_value(agent, reference);
    };
    let ReferenceBase::Value(base) = &reference.base else {
        return get_value(agent, reference);
    };
    get_property_key(agent, base, &reference.name, receiver.clone())
}

/// PutValue of a Reference, honoring a super reference's [[ThisValue]]
/// receiver: the property write is base.[[Set]](name, W, GetThisValue(V))
/// (spec 6.2.3.2 step 6.b).
fn put_reference_value(
    agent: &mut Agent,
    reference: &Reference,
    value: Value,
) -> Result<(), JsError> {
    // `put_value` resolves the receiver from [[ThisValue]] (super references)
    // and applies the module-namespace live-binding check; this super-set
    // path is just PutValue (spec 13.3.6.2).
    put_value(agent, reference, value)
}

/// An assignment target whose computed property key is converted lazily.
/// Evaluating `a[b] = c` leaves `b` unconverted until PutValue, after `c`
/// has run (spec 13.15.2 and EvaluatePropertyAccessWithExpressionKey's
/// note). The nullish-base TypeError also surfaces in PutValue.
pub(crate) enum LazyReference {
    /// An identifier or non-computed member target: nothing deferred.
    Reference(Reference),
    /// `base[key]`: both evaluated, the key still raw.
    Computed {
        base: Value,
        key: Value,
        strict: bool,
    },
    /// `super[key]`: the super base and `this` resolved, the key raw.
    SuperComputed {
        base: Value,
        this: Value,
        key: Value,
        strict: bool,
    },
}

impl LazyReference {
    /// PutValue, converting the deferred key first (spec 6.2.3.2 step 6.c:
    /// ToPropertyKey after ToObject).
    pub fn put(self, agent: &mut Agent, value: Value) -> Result<(), JsError> {
        match self {
            LazyReference::Reference(reference) => put_reference_value(agent, &reference, value),
            LazyReference::Computed { base, key, strict } => {
                let key = crate::context::to_property_key(agent, &key)?;
                let reference = Reference {
                    base: ReferenceBase::Value(base),
                    name: key,
                    strict,
                    this_value: None,
                    private_name: None,
                };
                put_value(agent, &reference, value)
            }
            LazyReference::SuperComputed {
                base,
                this,
                key,
                strict,
            } => {
                let key = crate::context::to_property_key(agent, &key)?;
                let reference = Reference {
                    base: ReferenceBase::Value(base),
                    name: key,
                    strict,
                    this_value: Some(this),
                    private_name: None,
                };
                put_reference_value(agent, &reference, value)
            }
        }
    }
}

/// Evaluate a LeftHandSideExpression as an assignment target, deferring the
/// nullish-base check and a computed member key's ToPropertyKey to
/// `LazyReference::put` (spec 13.15.2 and EvaluatePropertyAccessWith-
/// ExpressionKey's note).
pub(crate) fn eval_assignment_target(
    agent: &mut Agent,
    expr: &Expr,
    strict: bool,
) -> Result<LazyReference, JsError> {
    if let ExprKind::Paren(inner) = &expr.kind {
        return eval_assignment_target(agent, inner, strict);
    }
    if let ExprKind::Member(member) = &expr.kind {
        if let MemberProperty::Private(atom) = &member.property {
            // `obj.#name` — the private name resolves now; the brand check
            // happens in PrivateSet at PutValue.
            let name_id = crate::context::resolve_private_name(agent, *atom)?.id;
            let object = eval_chain(agent, &member.object, strict)?;
            let Some(object) = object else {
                return Err(JsError::new(
                    ErrorKind::ReferenceError,
                    "Invalid left-hand side in assignment".into(),
                ));
            };
            let base = match object {
                ChainResult::Reference(reference) => get_reference_value(agent, &reference)?,
                ChainResult::Value(value) => value,
            };
            return Ok(LazyReference::Reference(Reference {
                base: ReferenceBase::Value(base),
                name: PropertyKey::from_utf8(""),
                strict,
                this_value: None,
                private_name: Some(name_id),
            }));
        }
        if matches!(member.object.kind, ExprKind::Super) {
            let base = crate::context::get_super_base(agent)?;
            let this = crate::context::resolve_this_binding(agent)?;
            return match &member.property {
                MemberProperty::Computed(key_expr) => {
                    let key = eval_expr(agent, key_expr, strict)?;
                    Ok(LazyReference::SuperComputed {
                        base,
                        this,
                        key,
                        strict,
                    })
                }
                _ => {
                    let key = eval_member_key(agent, member, strict)?;
                    Ok(LazyReference::Reference(Reference {
                        base: ReferenceBase::Value(base),
                        name: key,
                        strict,
                        this_value: Some(this),
                        private_name: None,
                    }))
                }
            };
        }
        let object = eval_chain(agent, &member.object, strict)?;
        let Some(object) = object else {
            return Err(JsError::new(
                ErrorKind::ReferenceError,
                "Invalid left-hand side in assignment".into(),
            ));
        };
        let base = match object {
            ChainResult::Reference(reference) => get_reference_value(agent, &reference)?,
            ChainResult::Value(value) => value,
        };
        return match &member.property {
            MemberProperty::Computed(key_expr) => {
                let key = eval_expr(agent, key_expr, strict)?;
                Ok(LazyReference::Computed { base, key, strict })
            }
            _ => {
                let key = eval_member_key(agent, member, strict)?;
                Ok(LazyReference::Reference(Reference {
                    base: ReferenceBase::Value(base),
                    name: key,
                    strict,
                    this_value: None,
                    private_name: None,
                }))
            }
        };
    }
    let reference = eval_reference(agent, expr, strict)?;
    Ok(LazyReference::Reference(reference))
}

fn eval_literal(agent: &mut Agent, literal: &Literal) -> Result<Value, JsError> {
    match literal {
        Literal::Null => Ok(Value::Null),
        Literal::Boolean(b) => Ok(Value::Boolean(*b)),
        Literal::Number(n) => Ok(Value::Number(*n)),
        Literal::BigInt(n) => Ok(Value::BigInt(Handle::new(n.clone()))),
        Literal::Str(s) => Ok(Value::String(Handle::new(s.clone()))),
        Literal::RegExp { pattern, flags } => eval_regexp_literal(agent, pattern, flags),
    }
}

/// RegExpCreate (spec 22.2.4.6) from a literal; the lexer already validated
/// the pattern for early errors. A literal creates a fresh RegExp object per
/// evaluation.
pub(crate) fn eval_regexp_literal(
    agent: &mut Agent,
    pattern: &JsString,
    flags: &JsString,
) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let ctor = realm
        .intrinsics
        .get("%RegExp%")
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%RegExp% is not defined".into()))?;
    let args = vec![
        Value::String(Handle::new(pattern.clone())),
        Value::String(Handle::new(flags.clone())),
    ];
    crate::function::construct(agent, &ctor, &args, &ctor)
}

/// ArrayLiteral evaluation (spec 13.2.4.1): holes advance the index, spread
/// appends the iterated elements, and the final length is the element count.
fn eval_array_literal(
    agent: &mut Agent,
    literal: &syntax::ast::ArrayLiteral,
    strict: bool,
) -> Result<Value, JsError> {
    let array = crate::builtins::array::array_create(agent, 0.0)?;
    let mut index = 0usize;
    for element in &literal.elements {
        match element {
            ArrayElement::Hole => index += 1,
            ArrayElement::Expr(expr) => {
                let value = eval_expr(agent, expr, strict)?;
                array.create_data_property(&JsString::from_utf8(&index.to_string()), value)?;
                index += 1;
            }
            ArrayElement::Spread(expr) => {
                let iterable = eval_expr(agent, expr, strict)?;
                let iterator = get_iterator(agent, &iterable)?;
                while let Some(value) = iterator_step(agent, &iterator)? {
                    array.create_data_property(&JsString::from_utf8(&index.to_string()), value)?;
                    index += 1;
                }
            }
        }
    }
    array.set(
        &JsString::from_utf8("length"),
        Value::Number(index as f64),
        true,
    )?;
    Ok(Value::Object(array))
}

/// ObjectLiteral evaluation (spec 13.2.5.4): property definitions in order,
/// with the `__proto__` prototype-setter special case and spread.
fn eval_object_literal(
    agent: &mut Agent,
    literal: &ObjectLiteral,
    strict: bool,
) -> Result<Value, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| crate::context::as_object(&value));
    let object = crux::object::JsObject::ordinary_object_create(proto);
    for property in &literal.props {
        match property {
            ObjectProperty::Init {
                key,
                value: value_expr,
                shorthand,
            } => {
                let key = eval_property_name(agent, key, strict)?;
                let value = eval_expr(agent, value_expr, strict)?;
                if crate::function::is_anonymous_function_definition(value_expr) {
                    // spec 15.4.2 step 5: SetFunctionName from the property key.
                    crate::function::set_function_name(&value, &property_key_display(&key), None)?;
                }
                // Only a non-computed, non-shorthand `__proto__` key is the
                // prototype setter (Annex B.3.1); computed keys and shorthand
                // properties are ordinary own data properties, and duplicate
                // shorthand `__proto__` is permitted.
                let proto_setter = !shorthand
                    && !matches!(
                        property,
                        ObjectProperty::Init {
                            key: PropertyName::Computed(_),
                            ..
                        }
                    )
                    && matches!(&key, PropertyKey::String(id) if crux::lookup(*id).to_string_lossy() == "__proto__");
                if proto_setter {
                    match value.kind() {
                        ValueKind::Object(proto) => {
                            set_proto_or_throw(&object, Some(proto))?;
                        }
                        ValueKind::Null => {
                            set_proto_or_throw(&object, None)?;
                        }
                        // B.3.1 step 6: a non-object, non-null value sets
                        // neither the prototype nor an own property — the
                        // property definition is a no-op.
                        _ => {}
                    }
                } else {
                    object.create_data_property_key(&key, value)?;
                }
            }
            ObjectProperty::Method { key, function } => {
                // MethodDefinition evaluation (spec 15.4.3): OrdinaryFunction-
                // Create, MakeMethod, SetFunctionName, DefineMethodProperty.
                let key = eval_property_name(agent, key, strict)?;
                let env = agent.running_context()?.lexical_environment.clone();
                let closure = crate::function::instantiate_method(agent, function, env, strict)?;
                crate::function::make_method(agent, &closure, Value::Object(object.clone()))?;
                crate::function::set_function_name(&closure, &property_key_display(&key), None)?;
                object.create_data_property_key(&key, closure)?;
            }
            ObjectProperty::Get { key, body } => {
                // PropertyDefinition : get PropertyName ( ) { FunctionBody }
                let key = eval_property_name(agent, key, strict)?;
                let env = agent.running_context()?.lexical_environment.clone();
                let getter = crate::function::instantiate_accessor(
                    agent,
                    Vec::new(),
                    body.clone(),
                    env,
                    strict,
                )?;
                crate::function::make_method(agent, &getter, Value::Object(object.clone()))?;
                crate::function::set_function_name(
                    &getter,
                    &property_key_display(&key),
                    Some("get"),
                )?;
                object.define_property_key(
                    &key,
                    &crux::property::PropertyDescriptor {
                        value: None,
                        writable: None,
                        get: Some(getter),
                        set: None,
                        enumerable: Some(true),
                        configurable: Some(true),
                    },
                )?;
            }
            ObjectProperty::Set {
                key,
                param,
                init,
                body,
            } => {
                // PropertyDefinition : set PropertyName ( BindingElement ) { FunctionBody }
                let key = eval_property_name(agent, key, strict)?;
                let env = agent.running_context()?.lexical_environment.clone();
                let setter = crate::function::instantiate_accessor(
                    agent,
                    vec![BindingElement {
                        pattern: param.clone(),
                        init: init.clone(),
                        rest: false,
                        span: body.span,
                    }],
                    body.clone(),
                    env,
                    strict,
                )?;
                crate::function::make_method(agent, &setter, Value::Object(object.clone()))?;
                crate::function::set_function_name(
                    &setter,
                    &property_key_display(&key),
                    Some("set"),
                )?;
                object.define_property_key(
                    &key,
                    &crux::property::PropertyDescriptor {
                        value: None,
                        writable: None,
                        get: None,
                        set: Some(setter),
                        enumerable: Some(true),
                        configurable: Some(true),
                    },
                )?;
            }
            ObjectProperty::Spread(expr) => {
                let from = eval_expr(agent, expr, strict)?;
                copy_data_properties(agent, &object, &from)?;
            }
        }
    }
    Ok(Value::Object(object))
}

/// The `__proto__` prototype-setter step (Annex B.3.1): SetPrototypeOf, with
/// a TypeError when the object is non-extensible.
fn set_proto_or_throw(
    object: &crux::object::JsObject,
    proto: Option<crux::handle::Handle<crux::object::JsObject>>,
) -> Result<(), JsError> {
    if !object.set_prototype_of(proto)? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot set prototype of non-extensible object".into(),
        ));
    }
    Ok(())
}

/// CopyDataProperties (spec 14.1.16) for object-literal spread: copy every
/// enumerable own property of `from` onto `to`, overwriting any existing key
/// (a later spread overrides an earlier definition). Values are read with
/// `Get`, so accessor and proxy traps run even when the key already exists.
pub(crate) fn copy_data_properties(
    agent: &mut Agent,
    to: &crux::object::JsObject,
    from: &Value,
) -> Result<(), JsError> {
    if matches!(from.kind(), ValueKind::Null | ValueKind::Undefined) {
        return Ok(());
    }
    let ValueKind::Object(from_obj) = crate::context::to_object(agent, from)?.kind() else {
        return Ok(());
    };
    for key in from_obj.own_property_keys()? {
        let property = from_obj.get_own_property_key(&key)?;
        if let Some(property) = property {
            if !property.enumerable {
                continue;
            }
            let value = from_obj.get_key(&key)?;
            to.create_data_property_key(&key, value)?;
        }
    }
    Ok(())
}

/// The property name, evaluating computed keys (spec 13.2.5.5).
pub(crate) fn eval_property_name(
    agent: &mut Agent,
    key: &PropertyName,
    strict: bool,
) -> Result<PropertyKey, JsError> {
    match key {
        PropertyName::Ident(id) => Ok(PropertyKey::String(*id)),
        PropertyName::Str(text) => Ok(PropertyKey::from_js_string(text)),
        PropertyName::Number(n) => Ok(PropertyKey::String(crux::intern(
            to_string(&Value::Number(*n))?.as_slice(),
        ))),
        PropertyName::Computed(expr) => {
            let key = eval_expr(agent, expr, strict)?;
            crate::context::to_property_key(agent, &key)
        }
    }
}

/// The display name SetFunctionName uses for a key: the string, or a symbol's
/// bracketed description (empty when there is none) (spec 13.3.4 step 2).
pub(crate) fn property_key_display(key: &PropertyKey) -> JsString {
    match key {
        PropertyKey::String(id) => crux::lookup(*id),
        PropertyKey::Symbol(symbol) => match &symbol.description {
            Some(description) if !description.is_empty() => {
                JsString::from_utf8(&format!("[{}]", description.to_string_lossy()))
            }
            _ => JsString::from_utf8(""),
        },
    }
}

/// UnaryExpression evaluation (spec 13.5).
fn eval_unary(
    agent: &mut Agent,
    op: &UnaryOp,
    operand: &Expr,
    strict: bool,
) -> Result<Value, JsError> {
    match op {
        UnaryOp::Delete => {
            // delete of a non-reference (a literal, a parenthesized
            // expression, a short-circuited optional chain) returns true
            // (spec 13.5.1.2 step 3).
            // delete super.x / delete super[key] is a ReferenceError before
            // any ToPropertyKey of a computed key (spec 13.5.1.2 step 4.b):
            // the deferred-key evaluation keeps the key raw, so a key whose
            // toString throws still surfaces the ReferenceError
            // (super-property-topropertykey.js).
            if is_super_member_operand(operand) {
                let deferred = eval_assignment_target(agent, operand, strict)?;
                if matches!(deferred, LazyReference::SuperComputed { .. })
                    || matches!(
                        &deferred,
                        LazyReference::Reference(reference) if reference.this_value.is_some()
                    )
                {
                    return Err(JsError::new(
                        ErrorKind::ReferenceError,
                        "Unsupported reference to 'super'".into(),
                    ));
                }
            }
            let reference = match eval_chain(agent, operand, strict)? {
                Some(ChainResult::Reference(reference)) => reference,
                Some(ChainResult::Value(_)) | None => return Ok(Value::Boolean(true)),
            };
            // delete of a super property reference is a ReferenceError (spec
            // 13.5.1.2 step 5.b); the null base is never reached.
            if reference.this_value.is_some() {
                return Err(JsError::new(
                    ErrorKind::ReferenceError,
                    "Unsupported reference to 'super'".into(),
                ));
            }
            let deleted = delete_property_or_throw(agent, &reference)?;
            Ok(Value::Boolean(deleted))
        }
        UnaryOp::Void => {
            eval_expr(agent, operand, strict)?;
            Ok(Value::Undefined)
        }
        UnaryOp::Typeof => {
            // typeof on an unresolvable reference is "undefined", not an
            // error.
            let value = match eval_chain(agent, operand, strict)? {
                None => Value::Undefined,
                Some(ChainResult::Reference(reference)) => match &reference.base {
                    ReferenceBase::Unresolvable => Value::Undefined,
                    _ => get_reference_value(agent, &reference)?,
                },
                Some(ChainResult::Value(value)) => value,
            };
            // typeof null is "object" (spec 13.5.3.2 step 3); a proxy's
            // [[Call]] is fixed at creation, so a revoked callable proxy still
            // reads "function" (the crux type_of consults the revoked
            // target).
            let type_name = match value.kind() {
                ValueKind::Null => "object",
                ValueKind::Object(obj)
                    if matches!(
                        &obj.kind,
                        crux::object::ObjectKind::Proxy(slots) if slots.callable.get()
                    ) =>
                {
                    "function"
                }
                _ => type_of(&value),
            };
            Ok(Value::String(Handle::new(JsString::from_utf8(type_name))))
        }
        UnaryOp::Plus => {
            let value = eval_expr(agent, operand, strict)?;
            Ok(Value::Number(crate::context::to_number(agent, &value)?))
        }
        UnaryOp::Minus => {
            let value = eval_expr(agent, operand, strict)?;
            let numeric = to_numeric_operand(agent, &value)?;
            match numeric.kind() {
                ValueKind::Number(n) => Ok(Value::Number(-n)),
                ValueKind::BigInt(b) => Ok(Value::BigInt(Handle::new(bigint::unary_minus(&b)))),
                _ => unreachable!(),
            }
        }
        UnaryOp::BitNot => {
            let value = eval_expr(agent, operand, strict)?;
            let numeric = to_numeric_operand(agent, &value)?;
            match numeric.kind() {
                // ToInt32 (mod 2^32), not a saturating cast: ~-2147483649 is
                // ~2147483647 (S9.5_A2.1_T2).
                ValueKind::Number(n) => Ok(Value::Number((!to_int32(n)) as f64)),
                ValueKind::BigInt(b) => Ok(Value::BigInt(Handle::new(bigint::bitwise_not(&b)))),
                _ => unreachable!(),
            }
        }
        UnaryOp::Not => {
            let value = eval_expr(agent, operand, strict)?;
            Ok(Value::Boolean(!to_boolean(&value)))
        }
    }
}

/// The numeric/logical unary operators applied to an already-evaluated
/// operand (used by the resumable-function IR's `Unary` step).
pub(crate) fn eval_unary_value(
    agent: &mut Agent,
    op: &UnaryOp,
    value: Value,
) -> Result<Value, JsError> {
    match op {
        UnaryOp::Delete | UnaryOp::Void | UnaryOp::Typeof => Err(JsError::new(
            ErrorKind::SyntaxError,
            "unary operator is not a value tail".into(),
        )),
        UnaryOp::Plus => Ok(Value::Number(crate::context::to_number(agent, &value)?)),
        UnaryOp::Minus => {
            let numeric = to_numeric_operand(agent, &value)?;
            match numeric.kind() {
                ValueKind::Number(n) => Ok(Value::Number(-n)),
                ValueKind::BigInt(b) => Ok(Value::BigInt(Handle::new(bigint::unary_minus(&b)))),
                _ => unreachable!(),
            }
        }
        UnaryOp::BitNot => {
            let numeric = to_numeric_operand(agent, &value)?;
            match numeric.kind() {
                ValueKind::Number(n) => Ok(Value::Number((!to_int32(n)) as f64)),
                ValueKind::BigInt(b) => Ok(Value::BigInt(Handle::new(bigint::bitwise_not(&b)))),
                _ => unreachable!(),
            }
        }
        UnaryOp::Not => Ok(Value::Boolean(!to_boolean(&value))),
    }
}

/// UpdateExpression evaluation (spec 13.4.4-13.4.5): `++`/`--`.
fn eval_update(
    agent: &mut Agent,
    op: &UpdateOp,
    prefix: bool,
    target: &Expr,
    strict: bool,
) -> Result<Value, JsError> {
    let reference = eval_reference(agent, target, strict)?;
    let old = get_reference_value(agent, &reference)?;
    let old_numeric = to_numeric_operand(agent, &old)?;
    let new = match old_numeric.kind() {
        ValueKind::Number(n) => {
            let delta = if matches!(op, UpdateOp::Increment) {
                1.0
            } else {
                -1.0
            };
            Value::Number(n + delta)
        }
        ValueKind::BigInt(b) => {
            let one = crux::BigInt::from(1i64);
            let delta = if matches!(op, UpdateOp::Increment) {
                one
            } else {
                bigint::unary_minus(&one)
            };
            Value::BigInt(Handle::new(bigint::add(&b, &delta)))
        }
        _ => unreachable!(),
    };
    put_reference_value(agent, &reference, new.clone())?;
    // spec 13.4.4: a postfix update returns the old ToNumeric value (the
    // object is coerced, not returned as-is).
    if prefix { Ok(new) } else { Ok(old_numeric) }
}

/// LogicalExpression evaluation (spec 13.13.2-4): short-circuit on the left
/// value; `??` only falls through on nullish.
fn eval_logical(
    agent: &mut Agent,
    op: LogicalOp,
    left: &Expr,
    right: &Expr,
    strict: bool,
) -> Result<Value, JsError> {
    let left_value = eval_expr(agent, left, strict)?;
    let short_circuit = match op {
        LogicalOp::And => !to_boolean(&left_value),
        LogicalOp::Or => to_boolean(&left_value),
        LogicalOp::Nullish => !is_nullish(&left_value),
    };
    if short_circuit {
        Ok(left_value)
    } else {
        eval_expr(agent, right, strict)
    }
}

/// AssignmentExpression evaluation (spec 13.15.2): simple and compound
/// assignments to identifier and member references.
fn eval_assignment(
    agent: &mut Agent,
    op: &AssignOp,
    target: &Expr,
    value_expr: &Expr,
    strict: bool,
) -> Result<Value, JsError> {
    // Destructuring assignment targets (spec 13.15.4): an Array or Object
    // literal on the left of `=` is a pattern, not a value. Evaluate the RHS
    // first, then perform DestructuringAssignmentEvaluation; the result is
    // the RHS value.
    if matches!(op, AssignOp::Assign)
        && matches!(&target.kind, ExprKind::Array(_) | ExprKind::Object(_))
    {
        let value = eval_expr(agent, value_expr, strict)?;
        crate::binding::destructuring_assignment(agent, target, value.clone(), strict)?;
        return Ok(value);
    }
    if matches!(op, AssignOp::Assign) {
        // Simple assignment (spec 13.15.2): the target reference is evaluated
        // with its computed key conversion deferred past the RHS, and the
        // nullish-base TypeError surfaces in PutValue.
        let reference = eval_assignment_target(agent, target, strict)?;
        let value = named_eval_rhs(agent, target, value_expr, strict)?;
        reference.put(agent, value.clone())?;
        return Ok(value);
    }
    // Compound and logical assignments: GetValue(lref) runs before the RHS,
    // so the reference (and its key conversion) happens up front.
    let reference = eval_reference(agent, target, strict)?;
    match op {
        AssignOp::Assign => unreachable!("handled above"),
        AssignOp::AndAssign => {
            let old = get_reference_value(agent, &reference)?;
            if to_boolean(&old) {
                let new = named_eval_rhs(agent, target, value_expr, strict)?;
                put_reference_value(agent, &reference, new.clone())?;
                Ok(new)
            } else {
                Ok(old)
            }
        }
        AssignOp::OrAssign => {
            let old = get_reference_value(agent, &reference)?;
            if to_boolean(&old) {
                Ok(old)
            } else {
                let new = named_eval_rhs(agent, target, value_expr, strict)?;
                put_reference_value(agent, &reference, new.clone())?;
                Ok(new)
            }
        }
        AssignOp::NullishAssign => {
            let old = get_reference_value(agent, &reference)?;
            if is_nullish(&old) {
                let new = named_eval_rhs(agent, target, value_expr, strict)?;
                put_reference_value(agent, &reference, new.clone())?;
                Ok(new)
            } else {
                Ok(old)
            }
        }
        _ => {
            let old = get_reference_value(agent, &reference)?;
            let right = eval_expr(agent, value_expr, strict)?;
            let new = apply_compound(agent, *op, &old, &right)?;
            put_reference_value(agent, &reference, new.clone())?;
            Ok(new)
        }
    }
}

/// Evaluate the RHS of an assignment: when the target is an identifier and
/// the RHS is an anonymous function/class/arrow definition, it takes the
/// identifier as its name (spec 13.15.2 NamedEvaluation); otherwise the RHS
/// evaluates normally. An anonymous class expression is created with the
/// inferred name already applied, so its static field initializers observe it.
fn named_eval_rhs(
    agent: &mut Agent,
    target: &Expr,
    value_expr: &Expr,
    strict: bool,
) -> Result<Value, JsError> {
    let value = match &value_expr.kind {
        ExprKind::Class(class) if class.name.is_none() => {
            if let ExprKind::Ident(binding) = &target.kind {
                crate::class::class_definition_evaluation(agent, class, Some(*binding), strict)?
            } else {
                eval_expr(agent, value_expr, strict)?
            }
        }
        _ => eval_expr(agent, value_expr, strict)?,
    };
    if let ExprKind::Ident(name) = &target.kind
        && crate::function::is_anonymous_function_definition(value_expr)
    {
        let display = crate::function::default_binding_display_name(Some(crux::lookup(*name)))
            .unwrap_or_else(|| crux::lookup(*name));
        crate::function::set_function_name(&value, &display, None)?;
    }
    Ok(value)
}

/// Map a compound assignment operator onto its binary operator.
pub(crate) fn compound_binary(op: AssignOp) -> BinaryOp {
    match op {
        AssignOp::AddAssign => BinaryOp::Add,
        AssignOp::SubAssign => BinaryOp::Sub,
        AssignOp::MulAssign => BinaryOp::Mul,
        AssignOp::DivAssign => BinaryOp::Div,
        AssignOp::RemAssign => BinaryOp::Rem,
        AssignOp::ExpAssign => BinaryOp::Exp,
        AssignOp::LeftShiftAssign => BinaryOp::LeftShift,
        AssignOp::RightShiftAssign => BinaryOp::RightShift,
        AssignOp::UnsignedRightShiftAssign => BinaryOp::UnsignedRightShift,
        AssignOp::BitAndAssign => BinaryOp::BitAnd,
        AssignOp::BitXorAssign => BinaryOp::BitXor,
        AssignOp::BitOrAssign => BinaryOp::BitOr,
        AssignOp::Assign | AssignOp::AndAssign | AssignOp::OrAssign | AssignOp::NullishAssign => {
            unreachable!("handled above")
        }
    }
}

pub(crate) fn apply_compound(
    agent: &mut Agent,
    op: AssignOp,
    left: &Value,
    right: &Value,
) -> Result<Value, JsError> {
    apply_binary(agent, compound_binary(op), left, right)
}

fn mixed_bigint_error() -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        "Cannot mix BigInt and other types".into(),
    )
}

/// ToNumeric with agent-dispatched ToPrimitive for object operands (the
/// crux `to_numeric` cannot reach the valueOf/toString builtins).
fn to_numeric_operand(agent: &mut Agent, value: &Value) -> Result<Value, JsError> {
    if matches!(value.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        let prim = crate::context::to_primitive(agent, value, ToPrimitiveHint::Number)?;
        to_numeric(&prim)
    } else {
        to_numeric(value)
    }
}

/// ApplyStringOrNumericBinaryOperator (spec 13.15.4) for the arithmetic,
/// shift, and bitwise operators.
pub(crate) fn apply_binary(
    agent: &mut Agent,
    op: BinaryOp,
    left: &Value,
    right: &Value,
) -> Result<Value, JsError> {
    match op {
        BinaryOp::Add => {
            // Fast path: both operands are already numbers — direct double
            // addition without the ToPrimitive round-trips.
            if let (Some(left), Some(right)) = (left.as_number(), right.as_number()) {
                return Ok(Value::Number(left + right));
            }
            // Fast path: both operands are already strings — skip the
            // ToPrimitive/ToString round-trips (the Sputnik decodeURI
            // fixtures concatenate millions of small strings). The rope
            // concat appends without copying once the string is large.
            if let (Some(left_text), Some(right_text)) = (left.as_string(), right.as_string()) {
                return Ok(Value::String(JsString::concat(&left_text, &right_text)));
            }
            let left_prim = crate::context::to_primitive(agent, left, ToPrimitiveHint::Default)?;
            let right_prim = crate::context::to_primitive(agent, right, ToPrimitiveHint::Default)?;
            if matches!(left_prim.kind(), ValueKind::String(_))
                || matches!(right_prim.kind(), ValueKind::String(_))
            {
                // Concatenate at the UTF-16 unit level; a lossy Display path
                // would replace lone surrogates with U+FFFD.
                let left_text = crate::context::to_string(agent, &left_prim)?;
                let right_text = crate::context::to_string(agent, &right_prim)?;
                return Ok(Value::String(JsString::concat(
                    &Handle::new(left_text),
                    &Handle::new(right_text),
                )));
            }
            numeric_add(&left_prim, &right_prim)
        }
        BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem | BinaryOp::Exp => {
            numeric_binary(agent, op, left, right)
        }
        BinaryOp::LeftShift
        | BinaryOp::RightShift
        | BinaryOp::UnsignedRightShift
        | BinaryOp::BitAnd
        | BinaryOp::BitXor
        | BinaryOp::BitOr => bitwise_binary(agent, op, left, right),
        BinaryOp::LessThan => Ok(Value::Boolean(
            abstract_relational(agent, left, right, true)?.unwrap_or(false),
        )),
        BinaryOp::GreaterThan => Ok(Value::Boolean(
            abstract_relational(agent, right, left, false)?.unwrap_or(false),
        )),
        // x <= y is true only when IsLessThan(y, x) is false: an undefined
        // (NaN / incomparable) relation is false (spec 13.10.2).
        BinaryOp::LessEqual => Ok(Value::Boolean(matches!(
            abstract_relational(agent, right, left, false)?,
            Some(false)
        ))),
        BinaryOp::GreaterEqual => Ok(Value::Boolean(matches!(
            abstract_relational(agent, left, right, true)?,
            Some(false)
        ))),
        BinaryOp::Equal => Ok(Value::Boolean(abstract_loosely_equal(agent, left, right)?)),
        BinaryOp::NotEqual => Ok(Value::Boolean(!abstract_loosely_equal(agent, left, right)?)),
        BinaryOp::StrictEqual => Ok(Value::Boolean(is_strictly_equal(left, right))),
        BinaryOp::StrictNotEqual => Ok(Value::Boolean(!is_strictly_equal(left, right))),
        BinaryOp::In => {
            let key = crate::context::to_property_key(agent, left)?;
            match right.kind() {
                ValueKind::Object(obj) => Ok(Value::Boolean(
                    crate::module::has_property_with_deferred_trigger(agent, &obj, &key)?,
                )),
                ValueKind::Function(f) => Ok(Value::Boolean(
                    crate::module::has_property_with_deferred_trigger(agent, &f.object, &key)?,
                )),
                _ => Err(JsError::new(
                    ErrorKind::TypeError,
                    "Cannot use 'in' operator to search for a property in a non-object".into(),
                )),
            }
        }
        BinaryOp::Instanceof => {
            // InstanceofOperator (spec 7.3.20): an @@hasInstance method on the
            // right-hand side overrides the default prototype-chain walk.
            if !matches!(right.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Right-hand side of 'instanceof' is not an object".into(),
                ));
            }
            if let Some(handler) = get_method(agent, right, "@@hasInstance")? {
                let result = crate::function::call(
                    agent,
                    &handler,
                    right.clone(),
                    std::slice::from_ref(left),
                )?;
                return Ok(Value::Boolean(to_boolean(&result)));
            }
            if !is_callable(right) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Right-hand side of 'instanceof' is not callable".into(),
                ));
            }
            ordinary_has_instance(agent, right, left)
        }
    }
}

/// OrdinaryHasInstance (spec 7.3.19): walk the prototype chain of `value`
/// looking for `constructor.prototype`. `pub` so the built-in
/// %Function.prototype%[@@hasInstance] method can reuse it.
pub fn ordinary_has_instance(
    agent: &mut Agent,
    constructor: &Value,
    value: &Value,
) -> Result<Value, JsError> {
    if !is_callable(constructor) {
        return Ok(Value::Boolean(false));
    }
    // spec 7.3.19 step 2: a bound function delegates to its target.
    if let ValueKind::Function(function) = constructor.kind()
        && let crux::function::FunctionKind::Bound { target, .. } = &function.kind
    {
        return ordinary_has_instance(agent, target, value);
    }
    let Some(value_obj) = crate::context::as_object(value) else {
        return Ok(Value::Boolean(false));
    };
    let prototype = get_property(
        agent,
        constructor,
        &JsString::from_utf8("prototype"),
        constructor.clone(),
    )?;
    // Constructors hold their prototype as either an object value or (for
    // %Function%, whose `prototype` is the callable %Function.prototype%) a
    // function value; both carry an object handle for the walk.
    let Some(prototype_obj) = crate::context::as_object(&prototype) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Function has non-object prototype in instanceof check".into(),
        ));
    };
    let mut current = value_obj.get_prototype_of()?;
    while let Some(obj) = current {
        if same_value(
            &Value::Object(obj.clone()),
            &Value::Object(prototype_obj.clone()),
        ) {
            return Ok(Value::Boolean(true));
        }
        current = obj.get_prototype_of()?;
    }
    Ok(Value::Boolean(false))
}

/// The `+` operator after ToPrimitive: same-type numeric addition.
fn numeric_add(left: &Value, right: &Value) -> Result<Value, JsError> {
    match (left.kind(), right.kind()) {
        (ValueKind::BigInt(a), ValueKind::BigInt(b)) => {
            Ok(Value::BigInt(Handle::new(bigint::add(&a, &b))))
        }
        _ => {
            let left = to_number(left)?;
            let right = to_number(right)?;
            Ok(Value::Number(left + right))
        }
    }
}

/// spec 13.6-13.8 arithmetic with ToNumeric and same-type checks.
fn numeric_binary(
    agent: &mut Agent,
    op: BinaryOp,
    left: &Value,
    right: &Value,
) -> Result<Value, JsError> {
    // Fast path: both raw operands are already numbers — direct machine
    // arithmetic before the ToPrimitive/ToNumeric round-trips (ToNumeric on
    // a primitive is identity, so skipping it is exact). This is the hot
    // shape of arithmetic loops: the bench `i * 2` pays two tag checks here
    // instead of two `to_numeric_operand` calls.
    if let (Some(left_num), Some(right_num)) = (left.as_number(), right.as_number()) {
        let result = match op {
            BinaryOp::Sub => left_num - right_num,
            BinaryOp::Mul => left_num * right_num,
            BinaryOp::Div => left_num / right_num,
            BinaryOp::Rem => left_num % right_num,
            BinaryOp::Exp => number_exponentiate(left_num, right_num),
            _ => unreachable!("non-arithmetic op"),
        };
        return Ok(Value::Number(result));
    }
    let left = to_numeric_operand(agent, left)?;
    let right = to_numeric_operand(agent, right)?;
    // Fast path: plain doubles — direct machine arithmetic.
    if let (Some(left), Some(right)) = (left.as_number(), right.as_number()) {
        let result = match op {
            BinaryOp::Sub => left - right,
            BinaryOp::Mul => left * right,
            BinaryOp::Div => left / right,
            BinaryOp::Rem => left % right,
            BinaryOp::Exp => number_exponentiate(left, right),
            _ => unreachable!("non-arithmetic op"),
        };
        return Ok(Value::Number(result));
    }
    match (left.kind(), right.kind()) {
        (ValueKind::BigInt(a), ValueKind::BigInt(b)) => {
            let result = match op {
                BinaryOp::Sub => bigint::subtract(&a, &b),
                BinaryOp::Mul => bigint::multiply(&a, &b),
                BinaryOp::Div => {
                    if b.is_zero() {
                        return Err(JsError::new(
                            ErrorKind::RangeError,
                            "Division by zero".into(),
                        ));
                    }
                    bigint::divide(&a, &b)
                }
                BinaryOp::Rem => {
                    if b.is_zero() {
                        return Err(JsError::new(
                            ErrorKind::RangeError,
                            "Division by zero".into(),
                        ));
                    }
                    bigint::remainder(&a, &b)
                }
                BinaryOp::Exp => bigint::exponentiate(&a, &b)?,
                _ => unreachable!("non-arithmetic op"),
            };
            Ok(Value::BigInt(Handle::new(result)))
        }
        (ValueKind::Number(a), ValueKind::Number(b)) => {
            let result = match op {
                BinaryOp::Sub => a - b,
                BinaryOp::Mul => a * b,
                BinaryOp::Div => a / b,
                BinaryOp::Rem => a % b,
                BinaryOp::Exp => number_exponentiate(a, b),
                _ => unreachable!("non-arithmetic op"),
            };
            Ok(Value::Number(result))
        }
        (ValueKind::BigInt(_), ValueKind::Number(_))
        | (ValueKind::Number(_), ValueKind::BigInt(_)) => Err(mixed_bigint_error()),
        _ => unreachable!("ToNumeric produces Number or BigInt"),
    }
}

/// The BigInt shift count for the i64-based shift operators. Huge counts are
/// clamped so pathological shifts (e.g. `1n << 1e15n`) cannot exhaust
/// memory; the resulting bit patterns agree with the spec within the bound.
fn bigint_shift(count: &crux::BigInt) -> i64 {
    const SHIFT_LIMIT: f64 = 4_000_000.0;
    count.to_f64().clamp(-SHIFT_LIMIT, SHIFT_LIMIT) as i64
}

/// BigInt::leftShift/rightShift: a negative count divides, rounding down
/// toward -infinity (spec 6.1.6.2.10 step 1), while num_bigint's `/` rounds
/// toward zero (`-5n >> 1n` is -3n, not -2n).
fn bigint_shifted_floor(x: &crux::BigInt, shift: i64) -> crux::BigInt {
    if shift >= 0 {
        return crux::bigint::left_shift(x, shift);
    }
    let divisor = crux::BigInt::from(2u64).0.pow((-shift) as u32);
    let q = &x.0 / &divisor;
    if x.0 < &q * &divisor {
        // Rounding was upward: subtract one to reach the floor.
        crux::BigInt(&q - &crux::BigInt::from(1i64).0)
    } else {
        crux::BigInt(q)
    }
}

/// Number::exponentiate (spec 6.1.6.1.8): a NaN base or exponent is NaN
/// unless the exponent is ±0 (which yields 1), and an infinite exponent with
/// |base| = 1 is NaN (Rust's `powf` returns 1 for those cases).
fn number_exponentiate(base: f64, exponent: f64) -> f64 {
    if exponent == 0.0 {
        return 1.0;
    }
    if base.is_nan() || exponent.is_nan() {
        return f64::NAN;
    }
    if exponent.is_infinite() {
        let abs = base.abs();
        if abs > 1.0 {
            return if exponent > 0.0 { f64::INFINITY } else { 0.0 };
        }
        if abs == 1.0 {
            return f64::NAN;
        }
        return if exponent > 0.0 { 0.0 } else { f64::INFINITY };
    }
    base.powf(exponent)
}

/// ToInt32 (spec 7.1.6): truncate toward zero, reduce modulo 2^32; NaN and
/// the infinities map to 0 (Rust's `as` casts saturate instead).
fn to_int32(n: f64) -> i32 {
    if !n.is_finite() {
        return 0;
    }
    let wrapped = n.trunc() % 4294967296.0;
    (if wrapped < 0.0 {
        wrapped + 4294967296.0
    } else {
        wrapped
    }) as u32 as i32
}

/// ToUint32 (spec 7.1.7): like ToInt32, reinterpreting the low 32 bits.
fn to_uint32(n: f64) -> u32 {
    to_int32(n) as u32
}

/// spec 13.9 shifts and 13.11 bitwise operators.
fn bitwise_binary(
    agent: &mut Agent,
    op: BinaryOp,
    left: &Value,
    right: &Value,
) -> Result<Value, JsError> {
    // Fast path: both raw operands are already numbers — direct bit
    // arithmetic before the ToPrimitive/ToNumeric round-trips (identity for
    // primitives, so skipping them is exact).
    if let (Some(a), Some(b)) = (left.as_number(), right.as_number()) {
        let result = match op {
            BinaryOp::LeftShift => (to_int32(a) << (to_uint32(b) & 0x1F)) as f64,
            BinaryOp::RightShift => (to_int32(a) >> (to_uint32(b) & 0x1F)) as f64,
            BinaryOp::UnsignedRightShift => (to_uint32(a) >> (to_uint32(b) & 0x1F)) as f64,
            BinaryOp::BitAnd => (to_int32(a) & to_int32(b)) as f64,
            BinaryOp::BitXor => (to_int32(a) ^ to_int32(b)) as f64,
            BinaryOp::BitOr => (to_int32(a) | to_int32(b)) as f64,
            _ => unreachable!("non-bitwise op"),
        };
        return Ok(Value::Number(result));
    }
    let left = to_numeric_operand(agent, left)?;
    let right = to_numeric_operand(agent, right)?;
    match (left.kind(), right.kind()) {
        (ValueKind::BigInt(a), ValueKind::BigInt(b)) => {
            if matches!(op, BinaryOp::UnsignedRightShift) {
                return Err(mixed_bigint_error());
            }
            let result = match op {
                BinaryOp::LeftShift => bigint_shifted_floor(&a, bigint_shift(&b)),
                BinaryOp::RightShift => bigint_shifted_floor(&a, -bigint_shift(&b)),
                BinaryOp::UnsignedRightShift => unreachable!(),
                BinaryOp::BitAnd => bigint::bitwise_and(&a, &b),
                BinaryOp::BitXor => bigint::bitwise_xor(&a, &b),
                BinaryOp::BitOr => bigint::bitwise_or(&a, &b),
                _ => unreachable!("non-bitwise op"),
            };
            Ok(Value::BigInt(Handle::new(result)))
        }
        (ValueKind::Number(a), ValueKind::Number(b)) => {
            let result = match op {
                BinaryOp::LeftShift => (to_int32(a) << (to_uint32(b) & 0x1F)) as f64,
                BinaryOp::RightShift => (to_int32(a) >> (to_uint32(b) & 0x1F)) as f64,
                BinaryOp::UnsignedRightShift => (to_uint32(a) >> (to_uint32(b) & 0x1F)) as f64,
                BinaryOp::BitAnd => (to_int32(a) & to_int32(b)) as f64,
                BinaryOp::BitXor => (to_int32(a) ^ to_int32(b)) as f64,
                BinaryOp::BitOr => (to_int32(a) | to_int32(b)) as f64,
                _ => unreachable!("non-bitwise op"),
            };
            Ok(Value::Number(result))
        }
        (ValueKind::BigInt(_), ValueKind::Number(_))
        | (ValueKind::Number(_), ValueKind::BigInt(_)) => Err(mixed_bigint_error()),
        _ => unreachable!("ToNumeric produces Number or BigInt"),
    }
}

/// Abstract Relational Comparison (spec 7.2.10): `None` when a NaN makes the
/// relation undefined.
/// IsLessThan with the spec's `leftFirst` parameter (spec 7.2.11 steps 1-2):
/// `true` ToPrimitives the left operand first, `false` the right. The
/// relational operators swap arguments so the *source-left* operand's
/// valueOf/toString always runs first (S11.8.2_A2.3_T1).
fn abstract_relational(
    agent: &mut Agent,
    left: &Value,
    right: &Value,
    left_first: bool,
) -> Result<Option<bool>, JsError> {
    // Fast path: both operands are already numbers — direct comparison
    // without the ToPrimitive round-trips (ToPrimitive on a primitive is a
    // no-op, so skipping it is exact). This is the hot shape of loop tests.
    if let (Some(a), Some(b)) = (left.as_number(), right.as_number()) {
        if a.is_nan() || b.is_nan() {
            return Ok(None);
        }
        return Ok(Some(a < b));
    }
    if let (Some(a), Some(b)) = (left.as_string(), right.as_string()) {
        return Ok(Some(a.as_slice() < b.as_slice()));
    }
    let (left_prim, right_prim) = if left_first {
        let left_prim = crate::context::to_primitive(agent, left, ToPrimitiveHint::Number)?;
        let right_prim = crate::context::to_primitive(agent, right, ToPrimitiveHint::Number)?;
        (left_prim, right_prim)
    } else {
        let right_prim = crate::context::to_primitive(agent, right, ToPrimitiveHint::Number)?;
        let left_prim = crate::context::to_primitive(agent, left, ToPrimitiveHint::Number)?;
        (left_prim, right_prim)
    };
    // Fast path: plain doubles — direct comparison.
    if let (Some(a), Some(b)) = (left_prim.as_number(), right_prim.as_number()) {
        if a.is_nan() || b.is_nan() {
            return Ok(None);
        }
        return Ok(Some(a < b));
    }
    if let (Some(a), Some(b)) = (left_prim.as_string(), right_prim.as_string()) {
        return Ok(Some(a.as_slice() < b.as_slice()));
    }
    // spec 7.2.11 steps 4-5: a BigInt and a String compare by StringToBigInt;
    // a non-integer string makes the relation undefined. The type check uses
    // the ToPrimitive results, not the ToNumeric results below.
    if let (Some(a), Some(b)) = (left_prim.as_bigint(), right_prim.as_string()) {
        return Ok(crux::convert::string_to_bigint(&b).map(|ny| bigint::less_than(&a, &ny)));
    }
    if let (Some(a), Some(b)) = (left_prim.as_string(), right_prim.as_bigint()) {
        return Ok(crux::convert::string_to_bigint(&a).map(|nx| bigint::less_than(&nx, &b)));
    }
    let left_num = to_numeric(&left_prim)?;
    let right_num = to_numeric(&right_prim)?;
    match (left_num.kind(), right_num.kind()) {
        (ValueKind::BigInt(a), ValueKind::BigInt(b)) => Ok(Some(bigint::less_than(&a, &b))),
        (ValueKind::Number(a), ValueKind::Number(b)) => {
            if a.is_nan() || b.is_nan() {
                Ok(None)
            } else {
                Ok(Some(a < b))
            }
        }
        (ValueKind::BigInt(a), ValueKind::Number(b)) => {
            Ok(bigint_number_cmp(&a, b)?.map(|o| o == std::cmp::Ordering::Less))
        }
        (ValueKind::Number(a), ValueKind::BigInt(b)) => {
            Ok(bigint_number_cmp(&b, a)?.map(|o| o == std::cmp::Ordering::Greater))
        }
        _ => unreachable!("ToNumeric produces Number or BigInt"),
    }
}

/// IsLooselyEqual (spec 7.2.15): the object/object case is identity (same
/// type, spec step 1), so ToPrimitive runs only on the object side of a
/// mixed comparison (step 12) — the crux `is_loosely_equal` cannot dispatch
/// valueOf/toString.
fn abstract_loosely_equal(agent: &mut Agent, left: &Value, right: &Value) -> Result<bool, JsError> {
    // spec 7.2.15 steps 1-2: an [[IsHTMLDDA]] object is loosely equal to
    // null/undefined before any ToPrimitive runs.
    if is_htmldda(left) && matches!(right.kind(), ValueKind::Null | ValueKind::Undefined)
        || is_htmldda(right) && matches!(left.kind(), ValueKind::Null | ValueKind::Undefined)
    {
        return Ok(true);
    }
    let left_obj = matches!(left.kind(), ValueKind::Object(_) | ValueKind::Function(_));
    let right_obj = matches!(right.kind(), ValueKind::Object(_) | ValueKind::Function(_));
    if left_obj && right_obj {
        return Ok(is_strictly_equal(left, right));
    }
    let left_prim = if left_obj {
        crate::context::to_primitive(agent, left, ToPrimitiveHint::Default)?
    } else {
        left.clone()
    };
    let right_prim = if right_obj {
        crate::context::to_primitive(agent, right, ToPrimitiveHint::Default)?
    } else {
        right.clone()
    };
    // spec 7.2.15 steps 6-7: a BigInt and a String compare by StringToBigInt.
    // The crux `is_loosely_equal` uses a variant that rejects the empty
    // string, which StringToBigInt maps to 0n.
    if let (ValueKind::BigInt(a), ValueKind::String(s)) = (left_prim.kind(), right_prim.kind()) {
        return Ok(crux::convert::string_to_bigint(&s).is_some_and(|n| {
            is_strictly_equal(&Value::BigInt(Handle::new(n)), &Value::BigInt(a.clone()))
        }));
    }
    if let (ValueKind::String(s), ValueKind::BigInt(b)) = (left_prim.kind(), right_prim.kind()) {
        return Ok(crux::convert::string_to_bigint(&s).is_some_and(|n| {
            is_strictly_equal(&Value::BigInt(Handle::new(n)), &Value::BigInt(b.clone()))
        }));
    }
    crux::ops::is_loosely_equal(&left_prim, &right_prim)
}

/// Whether the value is the host's `$262.IsHTMLDDA` (Annex B.3.7).
fn is_htmldda(value: &Value) -> bool {
    matches!(
        value.kind(),
        ValueKind::Object(obj) if matches!(obj.kind, crux::object::ObjectKind::IsHTMLDDA)
    )
}

/// Compare a BigInt with a finite or infinite Number; `None` for NaN.
fn bigint_number_cmp(b: &crux::BigInt, n: f64) -> Result<Option<std::cmp::Ordering>, JsError> {
    use std::cmp::Ordering;
    if n.is_nan() {
        return Ok(None);
    }
    if n.is_infinite() {
        return Ok(Some(if n > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        }));
    }
    // b < n ⟺ b ≤ ⌈n⌉ − 1; b > n ⟺ b ≥ ⌊n⌋ + 1. The integer bounds are
    // exact f64→BigInt conversions.
    let ceil = crux::ops::f64_to_bigint_exact(n.ceil());
    if b.0 < ceil {
        return Ok(Some(Ordering::Less));
    }
    let floor = crux::ops::f64_to_bigint_exact(n.floor());
    if b.0 > floor {
        return Ok(Some(Ordering::Greater));
    }
    Ok(Some(Ordering::Equal))
}

/// Call evaluation (spec 13.3.6.1): evaluate arguments in order with spread.
fn eval_arguments(
    agent: &mut Agent,
    args: &[Argument],
    strict: bool,
) -> Result<Vec<Value>, JsError> {
    let mut values = Vec::with_capacity(args.len());
    for argument in args {
        match argument {
            Argument::Expr(expr) => values.push(eval_expr(agent, expr, strict)?),
            Argument::Spread(expr) => {
                let iterable = eval_expr(agent, expr, strict)?;
                let iterator = get_iterator(agent, &iterable)?;
                while let Some(value) = iterator_step(agent, &iterator)? {
                    values.push(value);
                }
            }
        }
    }
    Ok(values)
}

fn eval_call(
    agent: &mut Agent,
    call: &syntax::ast::CallExpr,
    strict: bool,
) -> Result<Value, JsError> {
    let result = eval_call_chain(agent, call, strict)?;
    match result {
        None => Ok(Value::Undefined),
        Some(ChainResult::Value(value)) => Ok(value),
        Some(ChainResult::Reference(_)) => unreachable!("calls evaluate to values"),
    }
}

/// NewExpression evaluation (spec 13.3.5): construct with the callee as
/// both constructor and newTarget.
fn eval_new(agent: &mut Agent, new: &syntax::ast::NewExpr, strict: bool) -> Result<Value, JsError> {
    let constructor = eval_expr(agent, &new.callee, strict)?;
    // EvaluateNew (spec 13.3.5.1.1): the argument list is evaluated before
    // the IsConstructor check, so `new x(x = Array)` runs the assignment
    // (ctorExpr-isCtor-after-args-eval.js).
    let args = eval_arguments(agent, &new.args, strict)?;
    if !crate::function::is_constructor(agent, &constructor) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("{} is not a constructor", type_of(&constructor)),
        ));
    }
    crate::function::construct(agent, &constructor, &args, &constructor)
}

/// TemplateLiteral evaluation (spec 13.3.7.3): concatenate the cooked quasis
/// with the stringified substitutions.
fn eval_template(
    agent: &mut Agent,
    template: &TemplateLiteral,
    strict: bool,
) -> Result<Value, JsError> {
    let mut text = String::new();
    for (index, quasi) in template.quasis.iter().enumerate() {
        let cooked = quasi
            .cooked
            .clone()
            .unwrap_or_else(|| JsString::from_utf8(""));
        text.push_str(&cooked.to_string_lossy());
        if let Some(expr) = template.exprs.get(index) {
            let value = eval_expr(agent, expr, strict)?;
            text.push_str(&crate::context::to_string(agent, &value)?.to_string_lossy());
        }
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

// The per-realm template-object cache (spec 12.2.9.3 GetTemplateObject):
// template objects are keyed by the parse node of the site. Within one parse
// the same site shares a node pointer, so a (parse, realm, node) key gives
// the spec's same-site identity. Node addresses can be reused across parses
// (a freed AST reallocated for the next `eval`), so each parse also gets a
// fresh generation: the node pointer alone would falsely cache across evals.
type TemplateCacheKey = (usize, usize, usize, usize);
thread_local! {
    static TEMPLATE_OBJECT_CACHE: std::cell::RefCell<Vec<(TemplateCacheKey, Value)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static TEMPLATE_PARSE_GENERATION: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Bump the parse generation: every fresh `parse_script`/`parse_module` is a
/// distinct site space for the template cache (each `eval` re-parses and gets
/// a fresh site).
pub fn bump_template_parse_generation() {
    TEMPLATE_PARSE_GENERATION.set(TEMPLATE_PARSE_GENERATION.get() + 1);
}

/// GetTemplateObject (spec 12.2.9.3): the realm's template cache, keyed by
/// parse-node identity and parse generation — the same source location yields
/// the same frozen array. Shared with the step VM's `TaggedTemplate`.
pub(crate) fn get_template_object(
    agent: &Agent,
    template: &TemplateLiteral,
) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let generation = TEMPLATE_PARSE_GENERATION.get();
    // The cache is keyed by source location, not parse-node identity: the
    // step VM embeds a clone of the template literal per compilation, so two
    // instances of the same factory-created function would otherwise miss.
    let span = template.span;
    let key = (
        generation,
        crux::handle::Handle::as_ptr(&realm) as usize,
        span.start as usize,
        span.end as usize,
    );
    let cached = TEMPLATE_OBJECT_CACHE.with(|cache| {
        cache
            .borrow()
            .iter()
            .find(|(cached_key, _)| *cached_key == key)
            .map(|(_, value)| value.clone())
    });
    if let Some(value) = cached {
        return Ok(value);
    }
    // GetTemplateObject (spec 12.2.9.3): index properties are
    // non-writable/non-configurable, `raw` is non-enumerable, and both
    // arrays are frozen.
    let obj = crux::object::JsObject::array_create(None, template.quasis.len() as f64)?;
    let raw = crux::object::JsObject::array_create(None, template.quasis.len() as f64)?;
    for (index, quasi) in template.quasis.iter().enumerate() {
        let key = &index.to_string();
        // A quasi whose TV is undefined (an invalid escape sequence)
        // yields the value *undefined*, not a string (spec 12.2.9.3).
        let cooked = match quasi.cooked.clone() {
            Some(cooked) => Value::String(Handle::new(cooked)),
            None => Value::Undefined,
        };
        obj.define_property_key(
            &PropertyKey::from_utf8(key),
            &PropertyDescriptor {
                value: Some(cooked),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(false),
            },
        )?;
        raw.define_property_key(
            &PropertyKey::from_utf8(key),
            &PropertyDescriptor {
                value: Some(Value::String(Handle::new(quasi.raw.clone()))),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(false),
            },
        )?;
    }
    raw.define_property_key(
        &PropertyKey::from_utf8("length"),
        &PropertyDescriptor {
            value: Some(Value::Number(template.quasis.len() as f64)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: None,
            configurable: Some(false),
        },
    )?;
    raw.prevent_extensions()?;
    obj.define_property_key(
        &PropertyKey::from_utf8("raw"),
        &PropertyDescriptor {
            value: Some(Value::Object(raw)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    obj.define_property_key(
        &PropertyKey::from_utf8("length"),
        &PropertyDescriptor {
            value: Some(Value::Number(template.quasis.len() as f64)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: None,
            configurable: Some(false),
        },
    )?;
    obj.prevent_extensions()?;
    let value = Value::Object(obj);
    TEMPLATE_OBJECT_CACHE.with(|cache| {
        cache.borrow_mut().push((key, value.clone()));
    });
    Ok(value)
}

fn eval_tagged_template(
    agent: &mut Agent,
    tag: &Expr,
    template: &TemplateLiteral,
    strict: bool,
) -> Result<Value, JsError> {
    // EvaluateCall with the tag reference (spec 13.3.11.1): `obj.fn`x keeps
    // `obj` as the call's this value.
    let tag_chain = eval_chain(agent, tag, strict)?;
    let (this, tag_value) = match tag_chain {
        Some(ChainResult::Reference(reference)) => {
            let this = get_this_value(&reference);
            (this, get_reference_value(agent, &reference)?)
        }
        Some(ChainResult::Value(value)) => (Value::Undefined, value),
        None => (Value::Undefined, Value::Undefined),
    };
    if !is_callable(&tag_value) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("{} is not a function", type_of(&tag_value)),
        ));
    }
    let template_object = get_template_object(agent, template)?;
    let mut args = vec![template_object];
    for expr in &template.exprs {
        args.push(eval_expr(agent, expr, strict)?);
    }
    crate::function::call(agent, &tag_value, this, &args)
}

/// The Iterator Record of GetIterator (spec 7.4.2).
#[derive(Debug, Clone)]
pub struct IteratorRecord {
    pub iterator: Value,
    pub next: Value,
}

/// GetIterator (spec 7.4.2): fetch `@@iterator`, invoke it, and extract the
/// `next` method.
pub fn get_iterator(agent: &mut Agent, value: &Value) -> Result<IteratorRecord, JsError> {
    let method = get_method(agent, value, "@@iterator")?;
    let Some(method) = method else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Value is not iterable".into(),
        ));
    };
    let iterator = crate::function::call(agent, &method, value.clone(), &[])?;
    if !matches!(iterator.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator must be an object".into(),
        ));
    }
    // The `next` method is fetched and cached here (the getter fires once,
    // spec 7.4.1-7.4.2), but a non-callable `next` only surfaces when it is
    // called: a `yield`/`await` may suspend between GetIterator and the first
    // step (spec 7.4.2 step 3 + 7.4.3).
    let next = get_property(
        agent,
        &iterator,
        &JsString::from_utf8("next"),
        iterator.clone(),
    )?;
    Ok(IteratorRecord { iterator, next })
}

/// The for-of iterator state at loop begin: the generic protocol record, or
/// the fast index path over a plain Array with the stock `@@iterator` (the
/// loop then re-reads `length` and each element per step, reproducing the
/// stock Array iterator's observable behavior without the iterator object,
/// the per-element `next()` call, or the result-object allocation).
pub enum ForOfState {
    Generic(IteratorRecord),
    FastArray(Value),
}

/// GetIterator for a `for-of` head, with the dense-Array fast path: the
/// `@@iterator` method is fetched exactly once (identical observable
/// behavior to [`get_iterator`]); when it is the intrinsic
/// `%Array.prototype.values%` over a plain Array, the fast state is
/// returned without even creating the iterator object (the stock iterator
/// is empty and unobservable). Any shadowed `@@iterator`, patched
/// `next`/`return` on the iterator chain, or non-plain-Array receiver keeps
/// the generic record.
pub fn for_of_begin(agent: &mut Agent, value: &Value) -> Result<ForOfState, JsError> {
    // Cut 27: per-array fast-verdict cell — the Cut 24 checks below re-ran
    // the own-@@iterator scan, the intrinsics lookups, and the proto walk
    // per begin (the bench begins the same array 100k times). The cell
    // records (array id, array generation, prototype id); a hit skips every
    // check except the cheap gen-validated stock-iterator probe (the
    // prototype's own mutations bump ITS generation, which the probe
    // re-validates). The array generation covers an own @@iterator addition
    // and proto changes (Cut 22's mechanism bumps it).
    if let ValueKind::Object(object) = value.kind()
        && matches!(object.kind, crux::object::ObjectKind::Array)
    {
        let index = object.id() as usize & (crate::ir::MEMBER_CELLS - 1);
        if let Some((cached_array, cached_generation, cached_proto)) =
            agent.for_of_array_cells[index]
            && cached_array == object.id()
            && cached_generation == object.generation()
            && let Ok(Some(proto)) = object.get_prototype_of()
            && proto.id() == cached_proto
            && for_of_fast_probe(agent, &proto)
        {
            return Ok(ForOfState::FastArray(value.clone()));
        }
    }
    // Cut 24: the fast-path verdict without the `get_method` chain walk —
    // a plain Array with no own @@iterator whose prototype is the realm's
    // %Array.prototype%, plus a gen-validated cached "the iterator
    // infrastructure is stock" verdict. Any doubt falls to the existing
    // generic path below (get_method included), which is unchanged.
    if let Some(realm) = agent.current_realm().ok()
        && let ValueKind::Object(object) = value.kind()
        && matches!(object.kind, crux::object::ObjectKind::Array)
        && object
            .get_own_property_key(&PropertyKey::Symbol(
                crux::symbol::well_known("iterator").as_ref().clone(),
            ))?
            .is_none()
        && let Some(ap_value) = realm.intrinsics.get("%Array.prototype%")
        && let ValueKind::Object(array_proto) = ap_value.kind()
        && object
            .get_prototype_of()?
            .as_ref()
            .is_some_and(|proto| proto.id() == array_proto.id())
        && (for_of_fast_probe(agent, &array_proto)
            || for_of_fast_resolve(agent, &array_proto)?.is_some())
    {
        let index = object.id() as usize & (crate::ir::MEMBER_CELLS - 1);
        agent.for_of_array_cells[index] =
            Some((object.id(), object.generation(), array_proto.id()));
        return Ok(ForOfState::FastArray(value.clone()));
    }
    let method = get_method(agent, value, "@@iterator")?;
    let Some(method) = method else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Value is not iterable".into(),
        ));
    };
    // Hoisted fast-path detection: a plain Array whose `@@iterator` is
    // still the intrinsic `%Array.prototype.values%` iterates by index in
    // the Vm with no iterator object at all. The stock iterator the generic
    // path would create has no own properties (CreateArrayIterator allocates
    // an empty object) and records exactly `(value, 0, Value)`, so its full
    // state — over `value` at index 0, intrinsic `next`, no `return` on the
    // %ArrayIteratorPrototype% → %Object.prototype% chain — is verified here
    // without the allocation, the `values()` call, or the array_iter_data
    // bookkeeping. Any divergence (a patched @@iterator/next/return, a
    // proxy, a non-Array receiver) falls through to the generic path, which
    // re-checks the created iterator as before.
    if let Some(realm) = agent.current_realm().ok()
        && matches!(
            value.kind(),
            ValueKind::Object(ref object)
                if matches!(object.kind, crux::object::ObjectKind::Array)
        )
        && realm
            .intrinsics
            .get("%Array.prototype[Symbol.iterator]%")
            .as_ref()
            == Some(&method)
    {
        let iterator_proto = realm.intrinsics.get("%ArrayIteratorPrototype%");
        if let Some(iterator_proto_value) = iterator_proto
            && let ValueKind::Object(iterator_proto) = iterator_proto_value.kind()
        {
            let intrinsic_next = realm.intrinsics.get("%ArrayIteratorPrototype.next%");
            let next_is_stock =
                match iterator_proto.get_own_property_key(&PropertyKey::from_utf8("next"))? {
                    Some(property) => property.value().as_ref() == intrinsic_next.as_ref(),
                    None => false,
                };
            if next_is_stock && !iterator_chain_has_return(agent, &iterator_proto_value)? {
                return Ok(ForOfState::FastArray(value.clone()));
            }
        }
    }
    let iterator = crate::function::call(agent, &method, value.clone(), &[])?;
    if !matches!(iterator.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator must be an object".into(),
        ));
    }
    let next = get_property(
        agent,
        &iterator,
        &JsString::from_utf8("next"),
        iterator.clone(),
    )?;
    // The stock values-iterator state: over a plain Array (a proxy whose
    // @@iterator resolves to the intrinsic would also create an entry — its
    // element reads must go through the traps, so it stays generic), at
    // index 0, in the value kind, with the intrinsic `next`.
    let stock = match iterator.kind() {
        ValueKind::Object(object) => {
            agent
                .array_iter_data
                .get(&object.id())
                .cloned()
                .filter(|(array, index, kind)| {
                    *index == 0
                        && *kind == crate::builtins::array::ArrayIterationKind::Value as u32
                        && matches!(
                            array.kind(),
                            ValueKind::Object(ref array_object)
                                if matches!(array_object.kind, crux::object::ObjectKind::Array)
                        )
                })
        }
        _ => None,
    };
    let intrinsic_next = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get("%ArrayIteratorPrototype.next%"));
    if let Some((array, _, _)) = stock
        && intrinsic_next.as_ref() == Some(&next)
        && !iterator_chain_has_return(agent, &iterator)?
    {
        return Ok(ForOfState::FastArray(array));
    }
    Ok(ForOfState::Generic(IteratorRecord { iterator, next }))
}

/// Whether a `return` property exists anywhere on `iterator`'s prototype
/// chain (own properties only, accessors never invoked): IteratorClose in
/// the generic path would call it on a break/return/error, so the for-of
/// fast path must not engage when one is present.
fn iterator_chain_has_return(agent: &mut Agent, iterator: &Value) -> Result<bool, JsError> {
    let key = PropertyKey::from_utf8("return");
    let mut probe = match iterator.kind() {
        ValueKind::Object(object) => Some(object),
        _ => None,
    };
    while let Some(object) = probe {
        if object.get_own_property_key(&key)?.is_some() {
            return Ok(true);
        }
        probe = object.prototype.borrow().clone();
    }
    let _ = agent;
    Ok(false)
}

/// The for-of fast-verdict probe (Cut 24): the cached "the Array-iteration
/// infrastructure is stock" verdict holds while the three shared objects'
/// generations match — a mutation anywhere (Array.prototype's @@iterator
/// patched, %ArrayIteratorPrototype%.next replaced, a `return` added to
/// the chain) bumps one and re-resolves.
fn for_of_fast_probe(agent: &mut Agent, array_proto: &crux::object::JsObject) -> bool {
    let index = array_proto.id() as usize & (crate::ir::MEMBER_CELLS - 1);
    let Some(verdict) = agent.for_of_fast_cells[index].as_ref() else {
        return false;
    };
    if verdict.array_proto.0 != array_proto.id()
        || verdict.array_proto.1 != array_proto.generation()
    {
        return false;
    }
    verdict.aip.1 == verdict.aip_handle.generation()
        && verdict.object_proto.1 == verdict.object_proto_handle.generation()
}

/// Resolve and cache the for-of fast verdict (Cut 24): %Array.prototype%'s
/// own @@iterator is the stock intrinsic, %ArrayIteratorPrototype% has the
/// stock `next`, and no `return` on the AIP → %Object.prototype% chain.
/// `Some(())` = stock (now cached); `None` = not (the caller falls to the
/// generic path).
fn for_of_fast_resolve(
    agent: &mut Agent,
    array_proto: &crux::object::JsObject,
) -> Result<Option<()>, JsError> {
    let Some(realm) = agent.current_realm().ok() else {
        return Ok(None);
    };
    let iterator_key = PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone());
    let stock = realm.intrinsics.get("%Array.prototype[Symbol.iterator]%");
    let Some(ap_iterator) = array_proto.get_own_property_key(&iterator_key)? else {
        return Ok(None);
    };
    if ap_iterator.value().as_ref() != stock.as_ref() {
        return Ok(None);
    }
    let Some(aip_value) = realm.intrinsics.get("%ArrayIteratorPrototype%") else {
        return Ok(None);
    };
    let ValueKind::Object(aip) = aip_value.kind() else {
        return Ok(None);
    };
    let intrinsic_next = realm.intrinsics.get("%ArrayIteratorPrototype.next%");
    let next_is_stock = match aip.get_own_property_key(&PropertyKey::from_utf8("next"))? {
        Some(property) => property.value().as_ref() == intrinsic_next.as_ref(),
        None => false,
    };
    if !next_is_stock || iterator_chain_has_return(agent, &aip_value)? {
        return Ok(None);
    }
    let Some(object_proto_value) = realm.intrinsics.get("%Object.prototype%") else {
        return Ok(None);
    };
    let ValueKind::Object(object_proto) = object_proto_value.kind() else {
        return Ok(None);
    };
    let index = array_proto.id() as usize & (crate::ir::MEMBER_CELLS - 1);
    agent.for_of_fast_cells[index] = Some(crate::agent::ForOfFastVerdict {
        array_proto: (array_proto.id(), array_proto.generation()),
        aip: (aip.id(), aip.generation()),
        aip_handle: aip,
        object_proto: (object_proto.id(), object_proto.generation()),
        object_proto_handle: object_proto,
    });
    Ok(Some(()))
}

/// The iterator's `next` method: the cached method from GetIterator, or a
/// TypeError when it is not callable (the error surfaces at the first call,
/// spec 7.4.2-7.4.3).
pub fn iterator_next_method(
    agent: &mut Agent,
    iterator: &IteratorRecord,
) -> Result<Value, JsError> {
    if is_callable(&iterator.next) {
        return Ok(iterator.next.clone());
    }
    let next = get_property(
        agent,
        &iterator.iterator,
        &JsString::from_utf8("next"),
        iterator.iterator.clone(),
    )?;
    if !is_callable(&next) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator's next method is not callable".into(),
        ));
    }
    Ok(next)
}

/// IteratorStep + IteratorValue (spec 7.4.5-7.4.6): the next value, or `None`
/// when the iterator is done.
pub fn iterator_step(
    agent: &mut Agent,
    iterator: &IteratorRecord,
) -> Result<Option<Value>, JsError> {
    let next = iterator_next_method(agent, iterator)?;
    let result = crate::function::call(agent, &next, iterator.iterator.clone(), &[])?;
    if !matches!(result.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator result is not an object".into(),
        ));
    }
    let done = get_property(agent, &result, &JsString::from_utf8("done"), result.clone())?;
    if to_boolean(&done) {
        return Ok(None);
    }
    let value = get_property(
        agent,
        &result,
        &JsString::from_utf8("value"),
        result.clone(),
    )?;
    Ok(Some(value))
}

/// IteratorClose (spec 7.4.11): invoke the iterator's `return` method when it
/// exists. Called with a normal completion: a non-object result is a TypeError
/// (step 8).
pub fn iterator_close(agent: &mut Agent, iterator: &IteratorRecord) -> Result<(), JsError> {
    iterator_close_inner(agent, iterator, false)
}

/// IteratorClose with a throw completion (spec 7.4.11 steps 6-7): the
/// original error wins, so a throwing `return` (or a throwing `return`
/// lookup) is swallowed and the result-object check is skipped.
pub fn iterator_close_throw(agent: &mut Agent, iterator: &IteratorRecord) -> Result<(), JsError> {
    let return_method = match get_property(
        agent,
        &iterator.iterator,
        &JsString::from_utf8("return"),
        iterator.iterator.clone(),
    ) {
        Ok(method) => method,
        Err(_) => return Ok(()),
    };
    if matches!(return_method.kind(), ValueKind::Undefined | ValueKind::Null) {
        return Ok(());
    }
    let _ = crate::function::call(agent, &return_method, iterator.iterator.clone(), &[]);
    Ok(())
}

pub(crate) fn iterator_close_inner(
    agent: &mut Agent,
    iterator: &IteratorRecord,
    completion_is_throw: bool,
) -> Result<(), JsError> {
    let return_method = get_property(
        agent,
        &iterator.iterator,
        &JsString::from_utf8("return"),
        iterator.iterator.clone(),
    )?;
    if matches!(return_method.kind(), ValueKind::Undefined | ValueKind::Null) {
        return Ok(());
    }
    let result = crate::function::call(agent, &return_method, iterator.iterator.clone(), &[])?;
    if !completion_is_throw && !matches!(result.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator result return is not an object".into(),
        ));
    }
    Ok(())
}

/// IteratorCloseAll over plain records: close in list order carrying the
/// completion; the first abrupt result becomes the completion and later
/// closes see a throw completion (their errors are swallowed).
pub fn iterator_close_all(agent: &mut Agent, iters: &[IteratorRecord]) -> Result<(), JsError> {
    let mut throw: Option<JsError> = None;
    for record in iters {
        match iterator_close_inner(agent, record, throw.is_some()) {
            Ok(()) => {}
            Err(e) => {
                if throw.is_none() {
                    throw = Some(e);
                }
            }
        }
    }
    match throw {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// GetMethod (spec 7.3.11) through a language value; the `@@iterator` key is
/// the well-known symbol.
pub fn get_method(
    agent: &mut Agent,
    value: &Value,
    symbol_name: &str,
) -> Result<Option<Value>, JsError> {
    // The `@@name` notation (spec 6.1.6.3.5) names the well-known symbol
    // `name`; the registry keys by the short name.
    let key = PropertyKey::Symbol(
        crux::symbol::well_known(symbol_name.trim_start_matches("@@"))
            .as_ref()
            .clone(),
    );
    let method = get_property_key(agent, value, &key, value.clone())?;
    match method.kind() {
        ValueKind::Undefined | ValueKind::Null => Ok(None),
        _ if is_callable(&method) => Ok(Some(method)),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            format!("{symbol_name} is not a function"),
        )),
    }
}
