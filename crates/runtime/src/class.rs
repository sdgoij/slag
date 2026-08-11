//! Class definitions (spec 15.7): ClassDefinitionEvaluation, the class
//! constructor, method/accessor definition on the prototype and constructor,
//! and instance/static field handling.

use crux::convert::{to_property_key, to_string};
use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::intern_utf8;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::Value;
use syntax::ast::{
    BindingElement, Block, Class, ClassElement, ClassElementName, Expr, Function as FunctionAst,
    PropertyName,
};

use crate::agent::Agent;
use crate::context::{ExecutionContext, get_property};
use crate::env::{EnvRef, new_declarative_environment, new_function_environment};
use crate::expr::eval_expr;
use crate::function::{
    ConstructorKind, instantiate_accessor, instantiate_class_constructor, instantiate_method,
    make_method, set_function_name,
};

/// ClassDefinitionEvaluation (spec 15.7.14). `class_binding` is the class
/// name for declarations (bound in the class scope AND returned for the
/// caller to initialize the outer binding) or named class expressions.
pub fn class_definition_evaluation(
    agent: &mut Agent,
    class: &Class,
    class_binding: Option<crux::string::AtomId>,
    strict: bool,
) -> Result<Value, JsError> {
    let env_record = agent.running_context()?.lexical_environment.clone();
    let class_env = new_declarative_environment(Some(env_record.clone()));
    let binding: Option<JsString> = class_binding.map(crux::lookup);
    if let Some(binding) = &binding {
        class_env.create_immutable_binding(binding, true)?;
    }

    // ClassHeritage (spec steps 12-20): evaluated with the class name
    // visible; the superclass must be a constructor or null.
    let (proto_parent, super_constructor) = match &class.heritage {
        None => (None, None),
        Some(expr) => {
            agent.running_context_mut()?.lexical_environment = class_env.clone();
            let superclass = eval_expr(agent, expr, strict)?;
            agent.running_context_mut()?.lexical_environment = env_record.clone();
            match superclass {
                Value::Null => (None, None),
                superclass if !crux::value::is_constructor(&superclass) => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Class extends value is not a constructor or null".into(),
                    ));
                }
                superclass => {
                    let prototype = get_property(
                        agent,
                        &superclass,
                        &JsString::from_utf8("prototype"),
                        superclass.clone(),
                    )?;
                    match prototype {
                        Value::Object(proto) => (Some(proto), Some(superclass)),
                        Value::Null => (None, Some(superclass)),
                        _ => {
                            return Err(JsError::new(
                                ErrorKind::TypeError,
                                "Class extends value does not have a valid prototype property"
                                    .into(),
                            ));
                        }
                    }
                }
            }
        }
    };

    let proto = JsObject::ordinary_object_create(proto_parent);

    // The constructor (spec steps 21-25): the ConstructorMethod of the body,
    // or a default constructor.
    let ctor_element = class.elements.iter().find_map(|element| match element {
        ClassElement::Method {
            is_static: false,
            name,
            function,
        } if is_constructor(name, function) => Some(function),
        _ => None,
    });
    let ctor = match ctor_element {
        Some(function) => instantiate_class_constructor(
            agent,
            function.params.clone(),
            function.body.clone(),
            class_env.clone(),
            true,
        )?,
        None => default_constructor(agent, super_constructor.is_some(), &class_env)?,
    };

    // SetFunctionName(ctor, className): the empty string for anonymous
    // class expressions (the enclosing binding renames it later).
    let class_name = binding.clone().unwrap_or_else(|| JsString::from_utf8(""));
    set_function_name(&ctor, &class_name, None)?;

    // MakeConstructor(ctor, false, proto) (spec steps 26-27): the class
    // `prototype` is non-writable; the prototype's `constructor` is defined
    // separately below (non-enumerable, writable).
    let Value::Function(ctor_handle) = &ctor else {
        return Ok(ctor);
    };
    ctor_handle.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(Value::Object(proto.clone())),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;

    // [[ConstructorKind]] = derived when the class has a heritage.
    if super_constructor.is_some()
        && let Some(data) = agent.ecma_functions.get_mut(&ctor_handle.id())
    {
        data.constructor_kind = ConstructorKind::Derived;
        data.super_constructor = super_constructor.clone();
    }

    // DefineMethodProperty(proto, "constructor", ctor, false).
    proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(ctor.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // The remaining class elements (spec steps 28-35): instance methods on
    // the prototype, static methods on the constructor, instance fields
    // collected into [[Fields]], static fields/blocks evaluated now.
    let mut fields = Vec::new();
    agent.running_context_mut()?.lexical_environment = class_env.clone();
    for element in &class.elements {
        if matches!(
            element,
            ClassElement::Method {
                is_static: false,
                name,
                function,
            } if is_constructor(name, function)
        ) {
            continue;
        }
        let is_static = element_is_static(element);
        let home = if is_static {
            ctor.clone()
        } else {
            Value::Object(proto.clone())
        };
        match element {
            ClassElement::Method { name, function, .. } => {
                let key = class_element_name(agent, name, strict)?;
                let closure = instantiate_method(agent, function, class_env.clone(), true)?;
                make_method(agent, &closure, home.clone())?;
                set_function_name(&closure, &key_string(&key), None)?;
                define_method_property(&home, &key, closure)?;
            }
            ClassElement::Get { name, body, .. } => {
                let key = class_element_name(agent, name, strict)?;
                let getter =
                    instantiate_accessor(agent, Vec::new(), body.clone(), class_env.clone(), true)?;
                make_method(agent, &getter, home.clone())?;
                set_function_name(&getter, &key_string(&key), Some("get"))?;
                define_accessor_property(&home, &key, Some(getter), None)?;
            }
            ClassElement::Set {
                name, param, body, ..
            } => {
                let key = class_element_name(agent, name, strict)?;
                let setter = instantiate_accessor(
                    agent,
                    vec![BindingElement {
                        pattern: param.clone(),
                        init: None,
                        rest: false,
                        span: body.span,
                    }],
                    body.clone(),
                    class_env.clone(),
                    true,
                )?;
                make_method(agent, &setter, home.clone())?;
                set_function_name(&setter, &key_string(&key), Some("set"))?;
                define_accessor_property(&home, &key, None, Some(setter))?;
            }
            ClassElement::Field { name, init, .. } => {
                let key = class_element_name(agent, name, strict)?;
                if is_static {
                    define_field(agent, &ctor, &key, init.as_ref())?;
                } else {
                    fields.push(crate::function::ClassField {
                        name: key,
                        init: init.clone(),
                        environment: class_env.clone(),
                    });
                }
            }
            ClassElement::StaticBlock(block) => {
                evaluate_static_block(agent, block, &ctor, &class_env)?;
            }
        }
    }
    agent.running_context_mut()?.lexical_environment = env_record.clone();

    // [[Fields]] (spec step 39): instance fields initialize per instance.
    if let Some(data) = agent.ecma_functions.get_mut(&ctor_handle.id()) {
        data.fields = fields;
    }

    // Initialize the class binding in the class scope (spec step 36).
    if let Some(binding) = &binding {
        class_env.initialize_binding(binding, ctor.clone())?;
    }

    Ok(ctor)
}

/// Whether a class element is the `constructor` method: a plain instance
/// method named `constructor`.
fn is_constructor(name: &ClassElementName, function: &FunctionAst) -> bool {
    matches!(
        name,
        ClassElementName::Property(PropertyName::Ident(atom)) if *atom == intern_utf8("constructor")
    ) && !function.is_async
        && !function.is_generator
}

/// IsStatic of a class element.
fn element_is_static(element: &ClassElement) -> bool {
    match element {
        ClassElement::Method { is_static, .. }
        | ClassElement::Get { is_static, .. }
        | ClassElement::Set { is_static, .. }
        | ClassElement::Field { is_static, .. } => *is_static,
        ClassElement::StaticBlock(_) => true,
    }
}

/// The default constructor (spec 15.7.14 step 23): `constructor() {}` for
/// base classes; a synthetic derived constructor that forwards the arguments
/// to `super` without the iterator protocol.
fn default_constructor(
    agent: &mut Agent,
    derived: bool,
    class_env: &EnvRef,
) -> Result<Value, JsError> {
    if derived {
        crate::function::instantiate_default_derived_constructor(agent, class_env.clone(), true)
    } else {
        instantiate_class_constructor(
            agent,
            Vec::new(),
            Block {
                stmts: Vec::new(),
                span: crux::Span::new(0, 0),
            },
            class_env.clone(),
            true,
        )
    }
}

/// The property key of a class element name, evaluating computed names
/// (spec 15.7.5 ClassElementName).
fn class_element_name(
    agent: &mut Agent,
    name: &ClassElementName,
    strict: bool,
) -> Result<PropertyKey, JsError> {
    match name {
        ClassElementName::Property(name) => property_name_key(agent, name, strict),
        ClassElementName::Private(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "private class elements are not implemented yet".into(),
        )),
    }
}

/// PropertyName as a property key (spec 13.2.5.5).
fn property_name_key(
    agent: &mut Agent,
    name: &PropertyName,
    strict: bool,
) -> Result<PropertyKey, JsError> {
    match name {
        PropertyName::Ident(id) => Ok(PropertyKey::String(*id)),
        PropertyName::Str(text) => Ok(PropertyKey::from_js_string(text)),
        PropertyName::Number(n) => Ok(PropertyKey::from_js_string(&to_string(&Value::Number(*n))?)),
        PropertyName::Computed(expr) => {
            let key = eval_expr(agent, expr, strict)?;
            to_property_key(&key)
        }
    }
}

/// The string used for SetFunctionName from a property key (spec 10.2.7
/// step 2: a symbol's description, or the string itself).
fn key_string(key: &PropertyKey) -> JsString {
    match key {
        PropertyKey::String(id) => crux::lookup(*id),
        PropertyKey::Symbol(_) => JsString::from_utf8(""),
    }
}

/// DefineMethodProperty (spec 15.7.14): a non-enumerable data property.
fn define_method_property(home: &Value, key: &PropertyKey, closure: Value) -> Result<(), JsError> {
    let Some(obj) = home_object(home) else {
        return Ok(());
    };
    obj.define_property_key(
        key,
        &PropertyDescriptor {
            value: Some(closure),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The accessor form of DefineMethodProperty: getters and setters with the
/// same key merge into one accessor property.
fn define_accessor_property(
    home: &Value,
    key: &PropertyKey,
    getter: Option<Value>,
    setter: Option<Value>,
) -> Result<(), JsError> {
    let Some(obj) = home_object(home) else {
        return Ok(());
    };
    obj.define_property_key(
        key,
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: getter,
            set: setter,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The object part of a method home: the class prototype or the constructor
/// (a function object).
fn home_object(home: &Value) -> Option<&crux::object::JsObject> {
    match home {
        Value::Object(obj) => Some(obj),
        Value::Function(function) => Some(&function.object),
        _ => None,
    }
}

/// DefineField (spec 7.3.23) for a static field: evaluate the initializer
/// with `this` = the constructor and create the data property now.
fn define_field(
    agent: &mut Agent,
    receiver: &Value,
    key: &PropertyKey,
    init: Option<&Expr>,
) -> Result<(), JsError> {
    let value = match init {
        Some(init) => evaluate_with_this(agent, receiver.clone(), |agent| {
            eval_expr(agent, init, true)
        })?,
        None => Value::Undefined,
    };
    let Some(obj) = home_object(receiver) else {
        return Ok(());
    };
    obj.create_data_property_or_throw_key(key, value)?;
    Ok(())
}

/// `static { ... }`: the block runs as a strict function body called with
/// the constructor as `this` (spec 15.7.14 step 30).
fn evaluate_static_block(
    agent: &mut Agent,
    block: &Block,
    ctor: &Value,
    class_env: &EnvRef,
) -> Result<(), JsError> {
    let function = FunctionAst {
        span: block.span,
        name: None,
        params: Vec::new(),
        body: block.clone(),
        is_async: false,
        is_generator: false,
    };
    let closure = instantiate_method(agent, &function, class_env.clone(), true)?;
    crate::function::call(agent, &closure, ctor.clone(), &[])?;
    Ok(())
}

/// Evaluate `f` with `this` bound to `this_value` in a fresh function
/// environment over the current lexical environment — used by static field
/// initializers whose `this` is the constructor.
fn evaluate_with_this(
    agent: &mut Agent,
    this_value: Value,
    f: impl FnOnce(&mut Agent) -> Result<Value, JsError>,
) -> Result<Value, JsError> {
    let old = agent.running_context()?.lexical_environment.clone();
    let realm = agent.running_context()?.realm.clone();
    let function_value = Value::Function(Function::new(None));
    let function_env =
        new_function_environment(Some(old), function_value.clone(), Value::Undefined, false);
    function_env.bind_this_value(this_value)?;
    agent.execution_context_stack.push(ExecutionContext {
        function: Some(function_value),
        realm,
        script_or_module: None,
        lexical_environment: function_env.clone(),
        variable_environment: function_env.clone(),
        private_environment: None,
    });
    let result = f(agent);
    agent.execution_context_stack.pop();
    result
}
