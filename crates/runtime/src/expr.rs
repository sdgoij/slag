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
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, is_callable, is_constructor, type_of};
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
            crate::function::instantiate_function_expression(agent, f, env, strict)
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
            // PrivateIn (spec 13.11.1): the `#name in obj` brand check.
            let name_id = crate::context::resolve_private_name(agent, *name)?.id;
            let object = eval_expr(agent, object, strict)?;
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
                Some(ChainResult::Reference(reference)) => get_value(agent, &reference),
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
                Err(JsError::new(
                    ErrorKind::TypeError,
                    "import.meta is not implemented until Phase 7".into(),
                ))
            }
        }
        ExprKind::ImportCall { specifier, options } => {
            let specifier = eval_expr(agent, specifier, strict)?;
            let options = match options {
                Some(expr) => Some(eval_expr(agent, expr, strict)?),
                None => None,
            };
            crate::module::dynamic_import(agent, &specifier, options.as_ref())
        }
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
        ExprKind::Member(member) => {
            if matches!(member.object.kind, ExprKind::Super) {
                // MakeSuperPropertyReference (spec 13.3.6.2): the base is the
                // method's [[HomeObject]] prototype, and calls through the
                // reference receive the current `this`.
                let base = crate::context::get_super_base(agent)?;
                if is_nullish(&base) {
                    return Err(nullish_member_error(member));
                }
                let name = eval_member_key(agent, member, strict)?;
                let this = crate::context::resolve_this_binding(agent)?;
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
                    ChainResult::Reference(reference) => get_value(agent, &reference)?,
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
                ChainResult::Reference(reference) => get_value(agent, &reference)?,
                ChainResult::Value(value) => value,
            };
            if member.optional && is_nullish(&object_value) {
                return Ok(None);
            }
            if !member.optional && is_nullish(&object_value) {
                return Err(nullish_member_error(member));
            }
            let name = eval_member_key(agent, member, strict)?;
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
            (this, get_value(agent, &reference)?)
        }
        ChainResult::Value(value) => (Value::Undefined, value),
    };
    if call.optional && is_nullish(&callee_value) {
        return Ok(None);
    }
    if !is_callable(&callee_value) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("{} is not a function", type_of(&callee_value)),
        ));
    }
    let args = eval_arguments(agent, &call.args, strict)?;
    // Direct eval (spec 13.3.6.1 step 5): a call whose callee is the
    // intrinsic %eval% reached through the identifier `eval` runs its first
    // argument as a Script; any other route to %eval% is an indirect eval.
    if is_eval_function(agent, &callee_value)? {
        let Some(source) = args.first() else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "eval requires a string argument".into(),
            ));
        };
        let source = to_string(source)?;
        let direct = matches!(
            call.callee.kind,
            ExprKind::Ident(id) if crux::lookup(id) == crux::string::JsString::from_utf8("eval")
        );
        let result = crate::script::perform_eval(agent, &source.to_string_lossy(), strict, direct)?;
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
    matches!(value, Value::Undefined | Value::Null)
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

fn eval_literal(agent: &mut Agent, literal: &Literal) -> Result<Value, JsError> {
    match literal {
        Literal::Null => Ok(Value::Null),
        Literal::Boolean(b) => Ok(Value::Boolean(*b)),
        Literal::Number(n) => Ok(Value::Number(*n)),
        Literal::BigInt(n) => Ok(Value::BigInt(Handle::new(n.clone()))),
        Literal::Str(s) => Ok(Value::String(Handle::new(s.clone()))),
        Literal::RegExp { pattern, flags } => {
            // RegExpCreate (spec 22.2.4.6) from a literal; the lexer already
            // validated the pattern for early errors.
            let realm = agent.current_realm()?;
            let ctor = realm.intrinsics.get("%RegExp%").ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "%RegExp% is not defined".into())
            })?;
            let args = vec![
                Value::String(Handle::new(pattern.clone())),
                Value::String(Handle::new(flags.clone())),
            ];
            crate::function::construct(agent, &ctor, &args, &ctor)
        }
    }
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
                ..
            } => {
                let key = eval_property_name(agent, key, strict)?;
                let value = eval_expr(agent, value_expr, strict)?;
                if crate::function::is_anonymous_function_definition(value_expr) {
                    // spec 15.4.2 step 5: SetFunctionName from the property key.
                    crate::function::set_function_name(&value, &property_key_display(&key), None)?;
                }
                let is_proto = matches!(&key, PropertyKey::String(id) if crux::lookup(*id).to_string_lossy() == "__proto__");
                if is_proto {
                    match value {
                        Value::Object(proto) => {
                            if !object.set_prototype_of(Some(proto))? {
                                return Err(JsError::new(
                                    ErrorKind::TypeError,
                                    "Cannot set prototype of non-extensible object".into(),
                                ));
                            }
                        }
                        Value::Null => {
                            if !object.set_prototype_of(None)? {
                                return Err(JsError::new(
                                    ErrorKind::TypeError,
                                    "Cannot set prototype of non-extensible object".into(),
                                ));
                            }
                        }
                        _ => {
                            object.create_data_property_key(
                                &PropertyKey::from_utf8("__proto__"),
                                value,
                            )?;
                        }
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
            ObjectProperty::Set { key, param, body } => {
                // PropertyDefinition : set PropertyName ( BindingElement ) { FunctionBody }
                let key = eval_property_name(agent, key, strict)?;
                let env = agent.running_context()?.lexical_environment.clone();
                let setter = crate::function::instantiate_accessor(
                    agent,
                    vec![BindingElement {
                        pattern: param.clone(),
                        init: None,
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
                copy_data_properties(&object, &from)?;
            }
        }
    }
    Ok(Value::Object(object))
}

/// CopyDataProperties (spec 14.1.16): copy the enumerable own properties of
/// `from` onto `to`, skipping keys that already exist.
pub(crate) fn copy_data_properties(
    to: &crux::object::JsObject,
    from: &Value,
) -> Result<(), JsError> {
    let Value::Object(from_obj) = from else {
        return Ok(());
    };
    for key in from_obj.own_property_keys()? {
        if to.has_own_property_key(&key)? {
            continue;
        }
        let property = from_obj.get_own_property_key(&key)?;
        if let Some(property) = property {
            if !property.enumerable {
                continue;
            }
            let value = match property.kind {
                crux::object::PropertyKind::Data { value, .. } => value,
                crux::object::PropertyKind::Accessor { .. } => from_obj.get_key(&key)?,
            };
            to.create_data_property_key(&key, value)?;
        }
    }
    Ok(())
}

/// The property name, evaluating computed keys (spec 13.2.5.5).
fn eval_property_name(
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

/// The display name SetFunctionName uses for a key: the string, or the
/// symbol's description (empty when there is none) (spec 13.3.4).
fn property_key_display(key: &PropertyKey) -> JsString {
    match key {
        PropertyKey::String(id) => crux::lookup(*id),
        PropertyKey::Symbol(symbol) => symbol
            .description
            .clone()
            .unwrap_or_else(|| JsString::from_utf8("")),
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
            let reference = eval_reference(agent, operand, strict)?;
            let deleted = delete_property_or_throw(&reference)?;
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
                    _ => get_value(agent, &reference)?,
                },
                Some(ChainResult::Value(value)) => value,
            };
            Ok(Value::String(Handle::new(JsString::from_utf8(type_of(
                &value,
            )))))
        }
        UnaryOp::Plus => {
            let value = eval_expr(agent, operand, strict)?;
            Ok(Value::Number(crate::context::to_number(agent, &value)?))
        }
        UnaryOp::Minus => {
            let value = eval_expr(agent, operand, strict)?;
            let numeric = to_numeric_operand(agent, &value)?;
            match numeric {
                Value::Number(n) => Ok(Value::Number(-n)),
                Value::BigInt(b) => Ok(Value::BigInt(Handle::new(bigint::unary_minus(&b)))),
                _ => unreachable!(),
            }
        }
        UnaryOp::BitNot => {
            let value = eval_expr(agent, operand, strict)?;
            let numeric = to_numeric_operand(agent, &value)?;
            match numeric {
                Value::Number(n) => Ok(Value::Number((!(n as i32)) as f64)),
                Value::BigInt(b) => Ok(Value::BigInt(Handle::new(bigint::bitwise_not(&b)))),
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
            match numeric {
                Value::Number(n) => Ok(Value::Number(-n)),
                Value::BigInt(b) => Ok(Value::BigInt(Handle::new(bigint::unary_minus(&b)))),
                _ => unreachable!(),
            }
        }
        UnaryOp::BitNot => {
            let numeric = to_numeric_operand(agent, &value)?;
            match numeric {
                Value::Number(n) => Ok(Value::Number((!(n as i32)) as f64)),
                Value::BigInt(b) => Ok(Value::BigInt(Handle::new(bigint::bitwise_not(&b)))),
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
    let old = get_value(agent, &reference)?;
    let old_numeric = to_numeric_operand(agent, &old)?;
    let new = match old_numeric {
        Value::Number(n) => {
            let delta = if matches!(op, UpdateOp::Increment) {
                1.0
            } else {
                -1.0
            };
            Value::Number(n + delta)
        }
        Value::BigInt(b) => {
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
    put_value(agent, &reference, new.clone())?;
    if prefix { Ok(new) } else { Ok(old) }
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
    let reference = eval_reference(agent, target, strict)?;
    match op {
        AssignOp::Assign => {
            let value = eval_expr(agent, value_expr, strict)?;
            // spec 13.15.2 step 1.e: an anonymous function assigned to an
            // identifier reference takes the identifier as its name.
            if let ExprKind::Ident(name) = &target.kind
                && crate::function::is_anonymous_function_definition(value_expr)
            {
                crate::function::set_function_name(&value, &crux::lookup(*name), None)?;
            }
            put_value(agent, &reference, value.clone())?;
            Ok(value)
        }
        AssignOp::AndAssign => {
            let old = get_value(agent, &reference)?;
            if to_boolean(&old) {
                let new = eval_expr(agent, value_expr, strict)?;
                put_value(agent, &reference, new.clone())?;
                Ok(new)
            } else {
                Ok(old)
            }
        }
        AssignOp::OrAssign => {
            let old = get_value(agent, &reference)?;
            if to_boolean(&old) {
                Ok(old)
            } else {
                let new = eval_expr(agent, value_expr, strict)?;
                put_value(agent, &reference, new.clone())?;
                Ok(new)
            }
        }
        AssignOp::NullishAssign => {
            let old = get_value(agent, &reference)?;
            if is_nullish(&old) {
                let new = eval_expr(agent, value_expr, strict)?;
                put_value(agent, &reference, new.clone())?;
                Ok(new)
            } else {
                Ok(old)
            }
        }
        _ => {
            let old = get_value(agent, &reference)?;
            let right = eval_expr(agent, value_expr, strict)?;
            let new = apply_compound(agent, *op, &old, &right)?;
            put_value(agent, &reference, new.clone())?;
            Ok(new)
        }
    }
}

/// Map a compound assignment operator onto its binary operator.
fn compound_binary(op: AssignOp) -> BinaryOp {
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
    if matches!(value, Value::Object(_) | Value::Function(_)) {
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
            let left_prim = crate::context::to_primitive(agent, left, ToPrimitiveHint::Default)?;
            let right_prim = crate::context::to_primitive(agent, right, ToPrimitiveHint::Default)?;
            if matches!(left_prim, Value::String(_)) || matches!(right_prim, Value::String(_)) {
                let text = format!(
                    "{}{}",
                    crate::context::to_string(agent, &left_prim)?,
                    crate::context::to_string(agent, &right_prim)?
                );
                return Ok(Value::String(Handle::new(JsString::from_utf8(&text))));
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
            abstract_relational(agent, left, right)?.unwrap_or(false),
        )),
        BinaryOp::GreaterThan => Ok(Value::Boolean(
            abstract_relational(agent, right, left)?.unwrap_or(false),
        )),
        BinaryOp::LessEqual => Ok(Value::Boolean(
            !abstract_relational(agent, right, left)?.unwrap_or(false),
        )),
        BinaryOp::GreaterEqual => Ok(Value::Boolean(
            !abstract_relational(agent, left, right)?.unwrap_or(false),
        )),
        BinaryOp::Equal => Ok(Value::Boolean(crux::ops::is_loosely_equal(left, right)?)),
        BinaryOp::NotEqual => Ok(Value::Boolean(!crux::ops::is_loosely_equal(left, right)?)),
        BinaryOp::StrictEqual => Ok(Value::Boolean(is_strictly_equal(left, right))),
        BinaryOp::StrictNotEqual => Ok(Value::Boolean(!is_strictly_equal(left, right))),
        BinaryOp::In => {
            let key = crate::context::to_property_key(agent, left)?;
            match right {
                Value::Object(obj) => Ok(Value::Boolean(obj.has_property_key(&key)?)),
                Value::Function(f) => Ok(Value::Boolean(f.object.has_property_key(&key)?)),
                _ => Err(JsError::new(
                    ErrorKind::TypeError,
                    "Cannot use 'in' operator to search for a property in a non-object".into(),
                )),
            }
        }
        BinaryOp::Instanceof => {
            // InstanceofOperator (spec 7.3.20): an @@hasInstance method on the
            // right-hand side overrides the default prototype-chain walk.
            if !matches!(right, Value::Object(_) | Value::Function(_)) {
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
    match (left, right) {
        (Value::BigInt(a), Value::BigInt(b)) => Ok(Value::BigInt(Handle::new(bigint::add(a, b)))),
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
    let left = to_numeric_operand(agent, left)?;
    let right = to_numeric_operand(agent, right)?;
    match (left, right) {
        (Value::BigInt(a), Value::BigInt(b)) => {
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
        (Value::Number(a), Value::Number(b)) => {
            let result = match op {
                BinaryOp::Sub => a - b,
                BinaryOp::Mul => a * b,
                BinaryOp::Div => a / b,
                BinaryOp::Rem => a % b,
                BinaryOp::Exp => a.powf(b),
                _ => unreachable!("non-arithmetic op"),
            };
            Ok(Value::Number(result))
        }
        (Value::BigInt(_), Value::Number(_)) | (Value::Number(_), Value::BigInt(_)) => {
            Err(mixed_bigint_error())
        }
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

/// spec 13.9 shifts and 13.11 bitwise operators.
fn bitwise_binary(
    agent: &mut Agent,
    op: BinaryOp,
    left: &Value,
    right: &Value,
) -> Result<Value, JsError> {
    let left = to_numeric_operand(agent, left)?;
    let right = to_numeric_operand(agent, right)?;
    match (left, right) {
        (Value::BigInt(a), Value::BigInt(b)) => {
            if matches!(op, BinaryOp::UnsignedRightShift) {
                return Err(mixed_bigint_error());
            }
            let result = match op {
                BinaryOp::LeftShift => bigint::left_shift(&a, bigint_shift(&b)),
                BinaryOp::RightShift => bigint::right_shift(&a, bigint_shift(&b)),
                BinaryOp::UnsignedRightShift => unreachable!(),
                BinaryOp::BitAnd => bigint::bitwise_and(&a, &b),
                BinaryOp::BitXor => bigint::bitwise_xor(&a, &b),
                BinaryOp::BitOr => bigint::bitwise_or(&a, &b),
                _ => unreachable!("non-bitwise op"),
            };
            Ok(Value::BigInt(Handle::new(result)))
        }
        (Value::Number(a), Value::Number(b)) => {
            let result = match op {
                BinaryOp::LeftShift => ((a as i32) << ((b as u32) & 0x1F)) as f64,
                BinaryOp::RightShift => ((a as i32) >> ((b as u32) & 0x1F)) as f64,
                // ToUint32's bit pattern: `x as u32` saturates negative f64s
                // to zero, so convert through i32 first (mod 2^32).
                BinaryOp::UnsignedRightShift => ((a as i32) as u32 >> ((b as u32) & 0x1F)) as f64,
                BinaryOp::BitAnd => ((a as i32) & (b as i32)) as f64,
                BinaryOp::BitXor => ((a as i32) ^ (b as i32)) as f64,
                BinaryOp::BitOr => ((a as i32) | (b as i32)) as f64,
                _ => unreachable!("non-bitwise op"),
            };
            Ok(Value::Number(result))
        }
        (Value::BigInt(_), Value::Number(_)) | (Value::Number(_), Value::BigInt(_)) => {
            Err(mixed_bigint_error())
        }
        _ => unreachable!("ToNumeric produces Number or BigInt"),
    }
}

/// Abstract Relational Comparison (spec 7.2.10): `None` when a NaN makes the
/// relation undefined.
fn abstract_relational(
    agent: &mut Agent,
    left: &Value,
    right: &Value,
) -> Result<Option<bool>, JsError> {
    let left_prim = crate::context::to_primitive(agent, left, ToPrimitiveHint::Number)?;
    let right_prim = crate::context::to_primitive(agent, right, ToPrimitiveHint::Number)?;
    if let (Value::String(a), Value::String(b)) = (&left_prim, &right_prim) {
        return Ok(Some(a.as_slice() < b.as_slice()));
    }
    let left_num = to_numeric(&left_prim)?;
    let right_num = to_numeric(&right_prim)?;
    match (&left_num, &right_num) {
        (Value::BigInt(a), Value::BigInt(b)) => Ok(Some(bigint::less_than(a, b))),
        (Value::Number(a), Value::Number(b)) => {
            if a.is_nan() || b.is_nan() {
                Ok(None)
            } else {
                Ok(Some(a < b))
            }
        }
        (Value::BigInt(a), Value::Number(b)) => {
            Ok(bigint_number_cmp(a, *b)?.map(|o| o == std::cmp::Ordering::Less))
        }
        (Value::Number(a), Value::BigInt(b)) => {
            Ok(bigint_number_cmp(b, *a)?.map(|o| o == std::cmp::Ordering::Greater))
        }
        _ => unreachable!("ToNumeric produces Number or BigInt"),
    }
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
    if !is_constructor(&constructor) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("{} is not a constructor", type_of(&constructor)),
        ));
    }
    let args = eval_arguments(agent, &new.args, strict)?;
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

/// TaggedTemplate evaluation (spec 13.3.6.2): build the template object
/// (cooked strings with a `raw` array) and call the tag with the
/// substitutions. The per-site template-object cache is Phase 8.
fn eval_tagged_template(
    agent: &mut Agent,
    tag: &Expr,
    template: &TemplateLiteral,
    strict: bool,
) -> Result<Value, JsError> {
    let tag_value = eval_expr(agent, tag, strict)?;
    if !is_callable(&tag_value) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("{} is not a function", type_of(&tag_value)),
        ));
    }
    let template_object = crux::object::JsObject::array_create(None, template.quasis.len() as f64)?;
    let raw = crux::object::JsObject::array_create(None, template.quasis.len() as f64)?;
    for (index, quasi) in template.quasis.iter().enumerate() {
        let cooked = quasi
            .cooked
            .clone()
            .unwrap_or_else(|| JsString::from_utf8(""));
        template_object.create_data_property(
            &JsString::from_utf8(&index.to_string()),
            Value::String(Handle::new(cooked)),
        )?;
        raw.create_data_property(
            &JsString::from_utf8(&index.to_string()),
            Value::String(Handle::new(quasi.raw.clone())),
        )?;
    }
    template_object.create_data_property(&JsString::from_utf8("raw"), Value::Object(raw))?;
    let mut args = vec![Value::Object(template_object)];
    for expr in &template.exprs {
        args.push(eval_expr(agent, expr, strict)?);
    }
    crate::function::call(agent, &tag_value, Value::Undefined, &args)
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
    if !matches!(iterator, Value::Object(_)) {
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
    if !matches!(result, Value::Object(_)) {
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
    if matches!(return_method, Value::Undefined | Value::Null) {
        return Ok(());
    }
    let _ = crate::function::call(agent, &return_method, iterator.iterator.clone(), &[]);
    Ok(())
}

fn iterator_close_inner(
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
    if matches!(return_method, Value::Undefined | Value::Null) {
        return Ok(());
    }
    let result = crate::function::call(agent, &return_method, iterator.iterator.clone(), &[])?;
    if !completion_is_throw && !matches!(result, Value::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator result return is not an object".into(),
        ));
    }
    Ok(())
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
    match method {
        Value::Undefined | Value::Null => Ok(None),
        v if is_callable(&v) => Ok(Some(v)),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            format!("{symbol_name} is not a function"),
        )),
    }
}
