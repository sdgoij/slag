//! Expression evaluation (spec ch. 13).
//!
//! Phase 6 evaluates every expression form: literals, identifiers, `this`,
//! array/object literals (with holes, spread, and `__proto__`), member
//! access and calls (with optional chaining), `new`, unary/update/binary/
//! logical/conditional/assignment/comma operators, and template literals.
//! Function/class/arrow expressions, destructuring, and `yield`/`await`
//! join with Phase 7.

use crux::bigint;
use crux::convert::{
    ToPrimitiveHint, to_boolean, to_number, to_numeric, to_primitive, to_property_key, to_string,
};
use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::ops::{is_strictly_equal, same_value};
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, is_callable, is_constructor, type_of};
use syntax::ast::{
    Argument, ArrayElement, AssignOp, BinaryOp, Expr, ExprKind, Literal, LogicalOp, MemberExpr,
    MemberProperty, ObjectLiteral, ObjectProperty, PropertyName, TemplateLiteral, UnaryOp,
    UpdateOp,
};

use crate::agent::Agent;
use crate::context::{
    Reference, ReferenceBase, delete_property_or_throw, get_property, get_property_key,
    get_this_value, get_value, put_value, resolve_binding,
};

/// Evaluate an expression to a value (spec 13.1.1).
pub fn eval_expr(agent: &mut Agent, expr: &Expr, strict: bool) -> Result<Value, JsError> {
    match &expr.kind {
        ExprKind::Literal(literal) => eval_literal(literal),
        ExprKind::Ident(name) => {
            let name = crux::lookup(*name);
            let reference = resolve_binding(agent, &name, strict)?;
            get_value(&reference)
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
        ExprKind::Class(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "class expressions are not implemented until later Phase 7 work".into(),
        )),
        ExprKind::Unary { op, operand } => eval_unary(agent, op, operand, strict),
        ExprKind::Update { op, prefix, target } => eval_update(agent, op, *prefix, target, strict),
        ExprKind::Binary { op, left, right } => {
            let left = eval_expr(agent, left, strict)?;
            let right = eval_expr(agent, right, strict)?;
            apply_binary(*op, &left, &right)
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
        ExprKind::Call(call) => eval_call(agent, call, strict),
        ExprKind::New(new) => eval_new(agent, new, strict),
        ExprKind::Member(_) => {
            let reference = eval_chain(agent, expr, strict)?;
            match reference {
                None => Ok(Value::Undefined),
                Some(ChainResult::Reference(reference)) => get_value(&reference),
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
                // new.target is *undefined* at the script level; functions
                // bind it in Phase 7.
                Ok(Value::Undefined)
            } else {
                Err(JsError::new(
                    ErrorKind::TypeError,
                    "import.meta is not implemented until Phase 7".into(),
                ))
            }
        }
        ExprKind::ImportCall { .. } => Err(JsError::new(
            ErrorKind::TypeError,
            "dynamic import is not implemented until Phase 7".into(),
        )),
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
            let object = eval_chain(agent, &member.object, strict)?;
            let Some(object) = object else {
                return Ok(None);
            };
            let object_value = match object {
                ChainResult::Reference(reference) => get_value(&reference)?,
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
    let callee = eval_chain(agent, &call.callee, strict)?;
    let Some(callee) = callee else {
        return Ok(None);
    };
    let (this, callee_value) = match callee {
        ChainResult::Reference(reference) => {
            let this = get_this_value(&reference);
            (this, get_value(&reference)?)
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
            to_property_key(&key)
        }
    }
}

/// Evaluate a LeftHandSideExpression to a Reference (spec 13.3.1), for
/// assignment targets and `delete`.
pub fn eval_reference(agent: &mut Agent, expr: &Expr, strict: bool) -> Result<Reference, JsError> {
    let chain = eval_chain(agent, expr, strict)?;
    match chain {
        Some(ChainResult::Reference(reference)) => Ok(reference),
        Some(ChainResult::Value(_)) | None => Err(JsError::new(
            ErrorKind::ReferenceError,
            "Invalid left-hand side in assignment".into(),
        )),
    }
}

fn eval_literal(literal: &Literal) -> Result<Value, JsError> {
    match literal {
        Literal::Null => Ok(Value::Null),
        Literal::Boolean(b) => Ok(Value::Boolean(*b)),
        Literal::Number(n) => Ok(Value::Number(*n)),
        Literal::BigInt(n) => Ok(Value::BigInt(Handle::new(n.clone()))),
        Literal::Str(s) => Ok(Value::String(Handle::new(s.clone()))),
        Literal::RegExp { .. } => Err(JsError::new(
            ErrorKind::TypeError,
            "Regular expression literals are not implemented until Phase 11".into(),
        )),
    }
}

/// ArrayLiteral evaluation (spec 13.2.4.1): holes advance the index, spread
/// appends the iterated elements, and the final length is the element count.
fn eval_array_literal(
    agent: &mut Agent,
    literal: &syntax::ast::ArrayLiteral,
    strict: bool,
) -> Result<Value, JsError> {
    let array = crux::object::JsObject::array_create(None, 0.0)?;
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
    let object = crux::object::JsObject::ordinary_object_create(None);
    for property in &literal.props {
        match property {
            ObjectProperty::Init { key, value, .. } => {
                let name = eval_property_name(agent, key, strict)?;
                let value = eval_expr(agent, value, strict)?;
                let name_text = name.to_string_lossy();
                if name_text == "__proto__" {
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
                            object
                                .create_data_property(&JsString::from_utf8("__proto__"), value)?;
                        }
                    }
                } else {
                    object.create_data_property(&name, value)?;
                }
            }
            ObjectProperty::Method { .. }
            | ObjectProperty::Get { .. }
            | ObjectProperty::Set { .. } => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "object literal methods and accessors are not implemented until Phase 7".into(),
                ));
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
fn copy_data_properties(to: &crux::object::JsObject, from: &Value) -> Result<(), JsError> {
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

/// The property name as a string, evaluating computed keys (spec 13.2.5.5).
fn eval_property_name(
    agent: &mut Agent,
    key: &PropertyName,
    strict: bool,
) -> Result<JsString, JsError> {
    match key {
        PropertyName::Ident(id) => Ok(crux::lookup(*id)),
        PropertyName::Str(text) => Ok(text.clone()),
        PropertyName::Number(n) => Ok(to_string(&Value::Number(*n))?),
        PropertyName::Computed(expr) => {
            let key = eval_expr(agent, expr, strict)?;
            let key = to_property_key(&key)?;
            match key {
                PropertyKey::String(id) => Ok(crux::lookup(id)),
                PropertyKey::Symbol(_) => Err(JsError::new(
                    ErrorKind::TypeError,
                    "symbol property keys are not supported in object literals yet".into(),
                )),
            }
        }
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
                    _ => get_value(&reference)?,
                },
                Some(ChainResult::Value(value)) => value,
            };
            Ok(Value::String(Handle::new(JsString::from_utf8(type_of(
                &value,
            )))))
        }
        UnaryOp::Plus => {
            let value = eval_expr(agent, operand, strict)?;
            Ok(Value::Number(to_number(&value)?))
        }
        UnaryOp::Minus => {
            let value = eval_expr(agent, operand, strict)?;
            let numeric = to_numeric(&value)?;
            match numeric {
                Value::Number(n) => Ok(Value::Number(-n)),
                Value::BigInt(b) => Ok(Value::BigInt(Handle::new(bigint::unary_minus(&b)))),
                _ => unreachable!(),
            }
        }
        UnaryOp::BitNot => {
            let value = eval_expr(agent, operand, strict)?;
            let numeric = to_numeric(&value)?;
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

/// UpdateExpression evaluation (spec 13.4.4-13.4.5): `++`/`--`.
fn eval_update(
    agent: &mut Agent,
    op: &UpdateOp,
    prefix: bool,
    target: &Expr,
    strict: bool,
) -> Result<Value, JsError> {
    let reference = eval_reference(agent, target, strict)?;
    let old = get_value(&reference)?;
    let old_numeric = to_numeric(&old)?;
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
    value: &Expr,
    strict: bool,
) -> Result<Value, JsError> {
    let reference = eval_reference(agent, target, strict)?;
    match op {
        AssignOp::Assign => {
            let value = eval_expr(agent, value, strict)?;
            put_value(agent, &reference, value.clone())?;
            Ok(value)
        }
        AssignOp::AndAssign => {
            let old = get_value(&reference)?;
            if to_boolean(&old) {
                let new = eval_expr(agent, value, strict)?;
                put_value(agent, &reference, new.clone())?;
                Ok(new)
            } else {
                Ok(old)
            }
        }
        AssignOp::OrAssign => {
            let old = get_value(&reference)?;
            if to_boolean(&old) {
                Ok(old)
            } else {
                let new = eval_expr(agent, value, strict)?;
                put_value(agent, &reference, new.clone())?;
                Ok(new)
            }
        }
        AssignOp::NullishAssign => {
            let old = get_value(&reference)?;
            if is_nullish(&old) {
                let new = eval_expr(agent, value, strict)?;
                put_value(agent, &reference, new.clone())?;
                Ok(new)
            } else {
                Ok(old)
            }
        }
        _ => {
            let old = get_value(&reference)?;
            let right = eval_expr(agent, value, strict)?;
            let new = apply_compound(*op, &old, &right)?;
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

fn apply_compound(op: AssignOp, left: &Value, right: &Value) -> Result<Value, JsError> {
    apply_binary(compound_binary(op), left, right)
}

fn mixed_bigint_error() -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        "Cannot mix BigInt and other types".into(),
    )
}

/// ApplyStringOrNumericBinaryOperator (spec 13.15.4) for the arithmetic,
/// shift, and bitwise operators.
fn apply_binary(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, JsError> {
    match op {
        BinaryOp::Add => {
            let left_prim = to_primitive(left, ToPrimitiveHint::Default)?;
            let right_prim = to_primitive(right, ToPrimitiveHint::Default)?;
            if matches!(left_prim, Value::String(_)) || matches!(right_prim, Value::String(_)) {
                let text = format!("{}{}", to_string(&left_prim)?, to_string(&right_prim)?);
                return Ok(Value::String(Handle::new(JsString::from_utf8(&text))));
            }
            numeric_add(&left_prim, &right_prim)
        }
        BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem | BinaryOp::Exp => {
            numeric_binary(op, left, right)
        }
        BinaryOp::LeftShift
        | BinaryOp::RightShift
        | BinaryOp::UnsignedRightShift
        | BinaryOp::BitAnd
        | BinaryOp::BitXor
        | BinaryOp::BitOr => bitwise_binary(op, left, right),
        BinaryOp::LessThan => Ok(Value::Boolean(
            abstract_relational(left, right)?.unwrap_or(false),
        )),
        BinaryOp::GreaterThan => Ok(Value::Boolean(
            abstract_relational(right, left)?.unwrap_or(false),
        )),
        BinaryOp::LessEqual => Ok(Value::Boolean(
            !abstract_relational(right, left)?.unwrap_or(false),
        )),
        BinaryOp::GreaterEqual => Ok(Value::Boolean(
            !abstract_relational(left, right)?.unwrap_or(false),
        )),
        BinaryOp::Equal => Ok(Value::Boolean(crux::ops::is_loosely_equal(left, right)?)),
        BinaryOp::NotEqual => Ok(Value::Boolean(!crux::ops::is_loosely_equal(left, right)?)),
        BinaryOp::StrictEqual => Ok(Value::Boolean(is_strictly_equal(left, right))),
        BinaryOp::StrictNotEqual => Ok(Value::Boolean(!is_strictly_equal(left, right))),
        BinaryOp::In => {
            let key = to_property_key(left)?;
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
            if !is_callable(right) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Right-hand side of 'instanceof' is not callable".into(),
                ));
            }
            ordinary_has_instance(right, left)
        }
    }
}

/// OrdinaryHasInstance (spec 7.3.19): walk the prototype chain of `value`
/// looking for `constructor.prototype`.
fn ordinary_has_instance(constructor: &Value, value: &Value) -> Result<Value, JsError> {
    if !is_callable(constructor) {
        return Ok(Value::Boolean(false));
    }
    let Value::Object(value_obj) = value else {
        return Ok(Value::Boolean(false));
    };
    let prototype = get_property(
        constructor,
        &JsString::from_utf8("prototype"),
        constructor.clone(),
    )?;
    if !matches!(prototype, Value::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Function has non-object prototype in instanceof check".into(),
        ));
    }
    let mut current = value_obj.get_prototype_of()?;
    while let Some(obj) = current {
        if same_value(&Value::Object(obj.clone()), &prototype) {
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
fn numeric_binary(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, JsError> {
    let left = to_numeric(left)?;
    let right = to_numeric(right)?;
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
fn bitwise_binary(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, JsError> {
    let left = to_numeric(left)?;
    let right = to_numeric(right)?;
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
fn abstract_relational(left: &Value, right: &Value) -> Result<Option<bool>, JsError> {
    let left_prim = to_primitive(left, ToPrimitiveHint::Number)?;
    let right_prim = to_primitive(right, ToPrimitiveHint::Number)?;
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
            text.push_str(&to_string(&value)?.to_string_lossy());
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
pub struct IteratorRecord {
    pub iterator: Value,
    pub next: Value,
}

/// GetIterator (spec 7.4.2): fetch `@@iterator`, invoke it, and extract the
/// `next` method.
pub fn get_iterator(agent: &mut Agent, value: &Value) -> Result<IteratorRecord, JsError> {
    let method = get_method(value, "@@iterator")?;
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
    let next = get_property(&iterator, &JsString::from_utf8("next"), iterator.clone())?;
    if !is_callable(&next) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator's next method is not callable".into(),
        ));
    }
    Ok(IteratorRecord { iterator, next })
}

/// IteratorStep + IteratorValue (spec 7.4.5-7.4.6): the next value, or `None`
/// when the iterator is done.
pub fn iterator_step(
    agent: &mut Agent,
    iterator: &IteratorRecord,
) -> Result<Option<Value>, JsError> {
    let result = crate::function::call(agent, &iterator.next, iterator.iterator.clone(), &[])?;
    if !matches!(result, Value::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator result is not an object".into(),
        ));
    }
    let done = get_property(&result, &JsString::from_utf8("done"), result.clone())?;
    if to_boolean(&done) {
        return Ok(None);
    }
    let value = get_property(&result, &JsString::from_utf8("value"), result.clone())?;
    Ok(Some(value))
}

/// IteratorClose (spec 7.4.7): invoke the iterator's `return` method when it
/// exists. Phase 6 propagates the close result; `throw`-completions integrate
/// in Phase 7.
pub fn iterator_close(agent: &mut Agent, iterator: &IteratorRecord) -> Result<(), JsError> {
    let return_method = get_property(
        &iterator.iterator,
        &JsString::from_utf8("return"),
        iterator.iterator.clone(),
    )?;
    if matches!(return_method, Value::Undefined | Value::Null) {
        return Ok(());
    }
    crate::function::call(agent, &return_method, iterator.iterator.clone(), &[])?;
    Ok(())
}

/// GetMethod (spec 7.3.11) through a language value; the `@@iterator` key is
/// the well-known symbol.
pub fn get_method(value: &Value, symbol_name: &str) -> Result<Option<Value>, JsError> {
    // The `@@name` notation (spec 6.1.6.3.5) names the well-known symbol
    // `name`; the registry keys by the short name.
    let key = PropertyKey::Symbol(
        crux::symbol::well_known(symbol_name.trim_start_matches("@@"))
            .as_ref()
            .clone(),
    );
    let method = get_property_key(value, &key, value.clone())?;
    match method {
        Value::Undefined | Value::Null => Ok(None),
        v if is_callable(&v) => Ok(Some(v)),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            format!("{symbol_name} is not a function"),
        )),
    }
}
