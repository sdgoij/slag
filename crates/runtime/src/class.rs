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
use crate::context::{ExecutionContext, get_property, new_private_environment, new_private_name};
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

    // The class PrivateEnvironment (spec steps 4-10): a fresh Private Name
    // per private identifier in the body, whose description is `#name`.
    let outer_private_env = agent.running_context()?.private_environment.clone();
    let class_private_env = new_private_environment(outer_private_env.clone());
    {
        let mut names = class_private_env.names.borrow_mut();
        for element in &class.elements {
            let Some(atom) = private_element_name(element) else {
                continue;
            };
            let description =
                JsString::from_utf8(&format!("#{}", crux::lookup(atom).to_string_lossy()));
            if !names.iter().any(|name| name.description == description) {
                names.push(new_private_name(description));
            }
        }
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
    set_private_environment(agent, &ctor, &class_private_env)?;

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
    // Set F.[[Prototype]] to superclass: static members of the superclass
    // (and its own statics, e.g. `Uint8Array.fromBase64`) resolve on the
    // subclass constructor (spec ClassDefinitionEvaluation step 29).
    if let Some(super_ctor) = &super_constructor {
        let super_object = match super_ctor {
            Value::Object(object) => object.clone(),
            Value::Function(function) => function.object.clone(),
            _ => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Superclass constructor is not an object".into(),
                ));
            }
        };
        ctor_handle.object.set_prototype_of(Some(super_object))?;
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
    // collected into [[Fields]], static fields/blocks collected for the
    // post-binding evaluation pass (steps 41-44).
    let mut fields = Vec::new();
    let mut instance_private_methods = Vec::new();
    let mut static_elements: Vec<StaticElement> = Vec::new();
    agent.running_context_mut()?.lexical_environment = class_env.clone();
    agent.running_context_mut()?.private_environment = Some(class_private_env.clone());
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
                let (private_id, key) = element_key(agent, name, strict)?;
                let closure = instantiate_method(agent, function, class_env.clone(), true)?;
                set_private_environment(agent, &closure, &class_private_env)?;
                make_method(agent, &closure, home.clone())?;
                set_function_name(&closure, &element_name_text(name), None)?;
                if let Some(name_id) = private_id {
                    // PrivateMethodOrAccessorAdd (spec 10.2.13).
                    let element = crux::object::PrivateElement {
                        name_id,
                        kind: crux::object::PrivateElementKind::Method(closure),
                    };
                    if is_static {
                        private_element_add(&home, element)?;
                    } else {
                        instance_private_methods.push(element);
                    }
                } else if let Some(key) = key {
                    define_method_property(&home, &key, closure)?;
                }
            }
            ClassElement::Get { name, body, .. } => {
                let (private_id, key) = element_key(agent, name, strict)?;
                let getter =
                    instantiate_accessor(agent, Vec::new(), body.clone(), class_env.clone(), true)?;
                set_private_environment(agent, &getter, &class_private_env)?;
                make_method(agent, &getter, home.clone())?;
                set_function_name(&getter, &element_name_text(name), Some("get"))?;
                if let Some(name_id) = private_id {
                    let element = crux::object::PrivateElement {
                        name_id,
                        kind: crux::object::PrivateElementKind::Accessor {
                            get: Some(getter),
                            set: None,
                        },
                    };
                    if is_static {
                        merge_private_accessor(&home, element)?;
                    } else {
                        merge_instance_private_accessor(&mut instance_private_methods, element);
                    }
                } else if let Some(key) = key {
                    define_accessor_property(&home, &key, Some(getter), None)?;
                }
            }
            ClassElement::Set {
                name, param, body, ..
            } => {
                let (private_id, key) = element_key(agent, name, strict)?;
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
                set_private_environment(agent, &setter, &class_private_env)?;
                make_method(agent, &setter, home.clone())?;
                set_function_name(&setter, &element_name_text(name), Some("set"))?;
                if let Some(name_id) = private_id {
                    let element = crux::object::PrivateElement {
                        name_id,
                        kind: crux::object::PrivateElementKind::Accessor {
                            get: None,
                            set: Some(setter),
                        },
                    };
                    if is_static {
                        merge_private_accessor(&home, element)?;
                    } else {
                        merge_instance_private_accessor(&mut instance_private_methods, element);
                    }
                } else if let Some(key) = key {
                    define_accessor_property(&home, &key, None, Some(setter))?;
                }
            }
            ClassElement::Field { name, init, .. } => {
                let (private_id, key) = element_key(agent, name, strict)?;
                if let Some(name_id) = private_id {
                    if is_static {
                        static_elements.push(StaticElement::Field {
                            key: None,
                            private_name: Some(name_id),
                            init: init.clone(),
                        });
                    } else {
                        fields.push(crate::function::ClassField {
                            name: PropertyKey::from_utf8(""),
                            private_name: Some(name_id),
                            init: init.clone(),
                            environment: class_env.clone(),
                        });
                    }
                } else if is_static {
                    static_elements.push(StaticElement::Field {
                        key: Some(key.unwrap()),
                        private_name: None,
                        init: init.clone(),
                    });
                } else {
                    fields.push(crate::function::ClassField {
                        name: key.unwrap(),
                        private_name: None,
                        init: init.clone(),
                        environment: class_env.clone(),
                    });
                }
            }
            ClassElement::StaticBlock(block) => {
                static_elements.push(StaticElement::Block(block.clone()));
            }
        }
    }
    agent.running_context_mut()?.lexical_environment = env_record.clone();
    agent.running_context_mut()?.private_environment = outer_private_env.clone();

    // [[Fields]], [[PrivateMethods]], and the class private environment (spec
    // steps 38-40).
    if let Some(data) = agent.ecma_functions.get_mut(&ctor_handle.id()) {
        data.fields = fields;
        data.private_methods = instance_private_methods;
        data.private_environment = Some(class_private_env.clone());
    }

    // Initialize the class binding in the class scope (spec step 36).
    if let Some(binding) = &binding {
        class_env.initialize_binding(binding, ctor.clone())?;
    }

    // Static fields and blocks evaluate after the class binding is
    // initialized, in source order (spec steps 41-44), with the class scope
    // and private names visible.
    agent.running_context_mut()?.lexical_environment = class_env.clone();
    agent.running_context_mut()?.private_environment = Some(class_private_env.clone());
    for element in &static_elements {
        match element {
            StaticElement::Field {
                key,
                private_name,
                init,
            } => {
                if let Some(name_id) = private_name {
                    let value = field_initializer(agent, init.as_ref(), &ctor)?;
                    private_field_add(&ctor, *name_id, value)?;
                } else {
                    define_field(agent, &ctor, key.as_ref().unwrap(), init.as_ref())?;
                }
            }
            StaticElement::Block(block) => {
                evaluate_static_block(agent, block, &ctor, &class_env, &class_private_env)?;
            }
        }
    }
    agent.running_context_mut()?.lexical_environment = env_record.clone();
    agent.running_context_mut()?.private_environment = outer_private_env.clone();

    Ok(ctor)
}

/// A static class element awaiting evaluation after the class binding is
/// initialized (spec 15.7.14 steps 41-44).
enum StaticElement {
    Field {
        key: Option<PropertyKey>,
        private_name: Option<u64>,
        init: Option<Expr>,
    },
    Block(Block),
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

/// The private name id and property key of a class element name (spec
/// 15.7.5): exactly one is present.
fn element_key(
    agent: &mut Agent,
    name: &ClassElementName,
    strict: bool,
) -> Result<(Option<u64>, Option<PropertyKey>), JsError> {
    match name {
        ClassElementName::Private(atom) => {
            let name = crate::context::resolve_private_name(agent, *atom)?;
            Ok((Some(name.id), None))
        }
        ClassElementName::Property(name) => {
            Ok((None, Some(property_name_key(agent, name, strict)?)))
        }
    }
}

/// The SetFunctionName string of a class element name (spec 10.2.7 step 2):
/// the property key string, or the `#name` description of a private name.
fn element_name_text(name: &ClassElementName) -> JsString {
    match name {
        ClassElementName::Private(atom) => {
            JsString::from_utf8(&format!("#{}", crux::lookup(*atom).to_string_lossy()))
        }
        ClassElementName::Property(name) => match name {
            PropertyName::Ident(id) => crux::lookup(*id),
            PropertyName::Str(text) => text.clone(),
            PropertyName::Number(n) => {
                to_string(&Value::Number(*n)).unwrap_or_else(|_| JsString::from_utf8(""))
            }
            PropertyName::Computed(_) => JsString::from_utf8(""),
        },
    }
}

/// The private identifier of a class element, if its name is private.
fn private_element_name(element: &ClassElement) -> Option<crux::string::AtomId> {
    let name = match element {
        ClassElement::Method { name, .. }
        | ClassElement::Get { name, .. }
        | ClassElement::Set { name, .. }
        | ClassElement::Field { name, .. } => name,
        ClassElement::StaticBlock(_) => return None,
    };
    match name {
        ClassElementName::Private(atom) => Some(*atom),
        ClassElementName::Property(_) => None,
    }
}

/// Attach the class PrivateEnvironment to a function's record so its body
/// can resolve `#name` (spec 10.2.1 [[PrivateEnvironment]]).
fn set_private_environment(
    agent: &mut Agent,
    function: &Value,
    private_env: &crux::handle::Handle<crate::context::PrivateEnvironment>,
) -> Result<(), JsError> {
    let Value::Function(function) = function else {
        return Ok(());
    };
    let Some(data) = agent.ecma_functions.get_mut(&function.id()) else {
        return Ok(());
    };
    data.private_environment = Some(private_env.clone());
    Ok(())
}

/// PrivateMethodOrAccessorAdd (spec 10.2.13) on a home object or the class
/// constructor.
fn private_element_add(home: &Value, element: crux::object::PrivateElement) -> Result<(), JsError> {
    let Some(obj) = home_object(home) else {
        return Ok(());
    };
    obj.private_element_add(element)
}

/// Merge a private accessor element into the instance list, combining
/// getter/setter pairs with the same name (spec 15.7.14 steps 33-37).
fn merge_instance_private_accessor(
    list: &mut Vec<crux::object::PrivateElement>,
    element: crux::object::PrivateElement,
) {
    if let Some(existing) = list
        .iter_mut()
        .find(|existing| existing.name_id == element.name_id)
        && let crux::object::PrivateElementKind::Accessor {
            get: existing_get,
            set: existing_set,
        } = &mut existing.kind
        && let crux::object::PrivateElementKind::Accessor { get, set } = element.kind
    {
        if get.is_some() {
            *existing_get = get;
        } else {
            *existing_set = set;
        }
    } else {
        list.push(element);
    }
}

/// Merge a static private accessor into the constructor's private elements.
fn merge_private_accessor(
    home: &Value,
    element: crux::object::PrivateElement,
) -> Result<(), JsError> {
    let Some(obj) = home_object(home) else {
        return Ok(());
    };
    if let Some(existing) = obj
        .private_elements
        .borrow_mut()
        .iter_mut()
        .find(|existing| existing.name_id == element.name_id)
        && let crux::object::PrivateElementKind::Accessor {
            get: existing_get,
            set: existing_set,
        } = &mut existing.kind
        && let crux::object::PrivateElementKind::Accessor { get, set } = element.kind
    {
        if get.is_some() {
            *existing_get = get;
        } else {
            *existing_set = set;
        }
    } else {
        obj.private_element_add(element)?;
    }
    Ok(())
}

/// PrivateFieldAdd (spec 10.2.10) on a receiver (object or constructor).
fn private_field_add(receiver: &Value, name_id: u64, value: Value) -> Result<(), JsError> {
    let Some(obj) = home_object(receiver) else {
        return Ok(());
    };
    obj.private_element_add(crux::object::PrivateElement {
        name_id,
        kind: crux::object::PrivateElementKind::Field(value),
    })
}

/// Evaluate a static field initializer with `this` = the constructor.
fn field_initializer(
    agent: &mut Agent,
    init: Option<&Expr>,
    receiver: &Value,
) -> Result<Value, JsError> {
    match init {
        Some(init) => evaluate_with_this(agent, receiver.clone(), |agent| {
            eval_expr(agent, init, true)
        }),
        None => Ok(Value::Undefined),
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
/// the constructor as `this` (spec 15.7.14 step 30), resolving the class's
/// private names.
fn evaluate_static_block(
    agent: &mut Agent,
    block: &Block,
    ctor: &Value,
    class_env: &EnvRef,
    class_private_env: &crux::handle::Handle<crate::context::PrivateEnvironment>,
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
    set_private_environment(agent, &closure, class_private_env)?;
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
    let source = agent
        .running_context()
        .ok()
        .and_then(|context| context.source.clone());
    agent.execution_context_stack.push(ExecutionContext {
        function: Some(function_value),
        realm,
        script_or_module: None,
        lexical_environment: function_env.clone(),
        variable_environment: function_env.clone(),
        private_environment: None,
        source,
    });
    let result = f(agent);
    agent.execution_context_stack.pop();
    result
}
