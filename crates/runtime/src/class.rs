//! Class definitions (spec 15.7): ClassDefinitionEvaluation, the class
//! constructor, method/accessor definition on the prototype and constructor,
//! and instance/static field handling.

use crux::convert::{to_property_key, to_string};
use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::intern_utf8;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind};
use syntax::ast::{
    BindingElement, Block, Class, ClassElement, ClassElementName, Expr, Function as FunctionAst,
    PropertyName,
};

use crate::agent::Agent;
use crate::context::{get_property, new_private_environment, new_private_name};
use crate::env::{EnvRef, new_declarative_environment};
use crate::expr::eval_expr;
use crate::function::{
    ConstructorKind, default_binding_display_name, instantiate_accessor,
    instantiate_class_constructor, instantiate_method, make_method, set_function_name,
};
use crate::ir::has_computed_public_name;

/// ClassDefinitionEvaluation (spec 15.7.14). `class_binding` is the class
/// name for declarations (bound in the class scope AND returned for the
/// caller to initialize the outer binding) or named class expressions.
pub fn class_definition_evaluation(
    agent: &mut Agent,
    class: &Class,
    class_binding: Option<crux::string::AtomId>,
    _enclosing_strict: bool,
) -> Result<Value, JsError> {
    // A class definition is always strict mode code (spec 15.7 note): the
    // heritage expression, computed keys, and element bodies run strict even
    // inside a sloppy script.
    let scope = setup_class_scope(agent, class, class_binding)?;
    let heritage = evaluate_heritage(agent, class, &scope)?;
    build_class(agent, class, class_binding, true, scope, heritage, None)
}

/// ClassDefinitionEvaluation driven by the resumable-function VM: the class
/// scope and PrivateEnvironment were established and the heritage (`None`
/// without an extends clause) and each computed element name were evaluated
/// in order, so the definition can suspend at `yield`/`await` inside them.
/// `keys` holds the per-element computed property keys.
pub fn class_definition_evaluation_with_keys(
    agent: &mut Agent,
    class: &Class,
    class_binding: Option<crux::string::AtomId>,
    heritage: Option<Value>,
    keys: &[Option<PropertyKey>],
) -> Result<Value, JsError> {
    let scope = setup_class_scope(agent, class, class_binding)?;
    let heritage = resolve_heritage(agent, heritage)?;
    build_class(
        agent,
        class,
        class_binding,
        true,
        scope,
        heritage,
        Some(keys),
    )
}

/// The VM path when the class scope was already established by `ClassBegin`
/// (so closures created in the heritage/computed-key expressions share the
/// environment whose class-name binding `build_class` initializes).
#[allow(clippy::too_many_arguments)]
pub fn class_definition_evaluation_with_scope(
    agent: &mut Agent,
    class: &Class,
    class_binding: Option<crux::string::AtomId>,
    class_env: EnvRef,
    class_private_env: Handle<crate::context::PrivateEnvironment>,
    outer_private_env: Option<Handle<crate::context::PrivateEnvironment>>,
    outer_env: EnvRef,
    heritage: Option<Value>,
    keys: &[Option<PropertyKey>],
) -> Result<Value, JsError> {
    let scope = ClassScope {
        class_env,
        class_private_env,
        outer_private_env,
        outer_env,
    };
    let heritage = resolve_heritage(agent, heritage)?;
    build_class(
        agent,
        class,
        class_binding,
        true,
        scope,
        heritage,
        Some(keys),
    )
}

/// The class scope environment, the class PrivateEnvironment, and the
/// environments to restore once the definition completes (spec 15.7.14 steps
/// 2-11).
struct ClassScope {
    class_env: EnvRef,
    class_private_env: crux::handle::Handle<crate::context::PrivateEnvironment>,
    outer_private_env: Option<crux::handle::Handle<crate::context::PrivateEnvironment>>,
    outer_env: EnvRef,
}

fn setup_class_scope(
    agent: &mut Agent,
    class: &Class,
    class_binding: Option<crux::string::AtomId>,
) -> Result<ClassScope, JsError> {
    let outer_env = agent.running_context()?.lexical_environment.clone();
    let class_env = new_declarative_environment(Some(outer_env.clone()));
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
    Ok(ClassScope {
        class_env,
        class_private_env,
        outer_private_env,
        outer_env,
    })
}

/// The resolved heritage of a class definition: the class prototype's parent
/// and the superclass constructor (spec 15.7.14 steps 12-20).
struct Heritage {
    proto_parent: Option<crux::handle::Handle<JsObject>>,
    super_constructor: Option<Value>,
}

/// Evaluate the heritage expression with the class scope active (the class
/// name binding is in TDZ there) and resolve it.
fn evaluate_heritage(
    agent: &mut Agent,
    class: &Class,
    scope: &ClassScope,
) -> Result<Heritage, JsError> {
    let superclass = match &class.heritage {
        None => None,
        Some(expr) => {
            agent.running_context_mut()?.lexical_environment = scope.class_env.clone();
            let superclass = eval_expr(agent, expr, true)?;
            agent.running_context_mut()?.lexical_environment = scope.outer_env.clone();
            Some(superclass)
        }
    };
    resolve_heritage(agent, superclass)
}

/// Resolve an already-evaluated heritage value: the superclass must be a
/// constructor or *null*, and the class prototype inherits %Object.prototype%
/// unless the heritage supplies a prototype (spec 15.7.14 steps 12-20).
fn resolve_heritage(agent: &mut Agent, superclass: Option<Value>) -> Result<Heritage, JsError> {
    let Some(superclass) = superclass else {
        let object_proto = agent
            .current_realm()?
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| crate::context::as_object(&value));
        return Ok(Heritage {
            proto_parent: object_proto,
            super_constructor: None,
        });
    };
    match superclass.kind() {
        ValueKind::Null => {
            // Spec 15.7.14: ctorParent is %Function.prototype% and the class
            // is still derived (step 31), so `this` stays uninitialized until
            // `super()` (which then throws — %Function.prototype% is not a
            // constructor).
            let ctor_parent = agent
                .current_realm()?
                .intrinsics
                .get("%Function.prototype%");
            Ok(Heritage {
                proto_parent: None,
                super_constructor: ctor_parent,
            })
        }
        _ if !crate::function::is_constructor(agent, &superclass) => Err(JsError::new(
            ErrorKind::TypeError,
            "Class extends value is not a constructor or null".into(),
        )),
        _ => {
            let prototype = get_property(
                agent,
                &superclass,
                &JsString::from_utf8("prototype"),
                superclass.clone(),
            )?;
            match prototype.kind() {
                ValueKind::Object(proto) => Ok(Heritage {
                    proto_parent: Some(proto),
                    super_constructor: Some(superclass.clone()),
                }),
                // A callable prototype (Function.prototype) is still an
                // ordinary object for prototype purposes.
                ValueKind::Function(proto) => Ok(Heritage {
                    proto_parent: Some(proto.object.clone()),
                    super_constructor: Some(superclass.clone()),
                }),
                ValueKind::Null => Ok(Heritage {
                    proto_parent: None,
                    super_constructor: Some(superclass.clone()),
                }),
                _ => Err(JsError::new(
                    ErrorKind::TypeError,
                    "Class extends value does not have a valid prototype property".into(),
                )),
            }
        }
    }
}

/// Build the constructor and its prototype (spec 15.7.14 steps 21-44). When
/// `precomputed_keys` is present the computed element names were already
/// evaluated (the resumable-VM path); otherwise they are evaluated inline.
fn build_class(
    agent: &mut Agent,
    class: &Class,
    class_binding: Option<crux::string::AtomId>,
    strict: bool,
    scope: ClassScope,
    heritage: Heritage,
    precomputed_keys: Option<&[Option<PropertyKey>]>,
) -> Result<Value, JsError> {
    let ClassScope {
        class_env,
        class_private_env,
        outer_private_env,
        outer_env: env_record,
    } = scope;
    let Heritage {
        proto_parent,
        super_constructor,
    } = heritage;
    let binding: Option<JsString> = class_binding.map(crux::lookup);

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
        Some(function) => {
            let ctor_source = crate::function::capture_source(agent, function.span);
            instantiate_class_constructor(
                agent,
                function.params.clone(),
                crate::function::shared_function_body(agent, function, ctor_source.as_ref()),
                class_env.clone(),
                true,
            )?
        }
        None => default_constructor(agent, super_constructor.is_some(), &class_env)?,
    };
    set_private_environment(agent, &ctor, &class_private_env)?;
    // MakeMethod(constructor, proto): the constructor's [[HomeObject]] lets
    // `super.prop` resolve inside it and in arrows created by field
    // initializers (spec 15.7.11 step 24.c); the default constructor gets the
    // same home so fields on a class without an explicit constructor resolve
    // `super` too.
    make_method(agent, &ctor, Value::Object(proto.clone()))?;

    // SetFunctionName(ctor, className): the empty string for anonymous
    // class expressions (the enclosing binding renames it later); a default
    // export's `*default*` binding is renamed to "default" (spec 15.2.3.11).
    let class_name =
        default_binding_display_name(binding.clone()).unwrap_or_else(|| JsString::from_utf8(""));
    set_function_name(&ctor, &class_name, None)?;

    // MakeConstructor(ctor, false, proto) (spec steps 26-27): the class
    // `prototype` is non-writable; the prototype's `constructor` is defined
    // separately below (non-enumerable, writable).
    let ValueKind::Function(ctor_handle) = ctor.kind() else {
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
        let super_object = match super_ctor.kind() {
            ValueKind::Object(object) => object,
            ValueKind::Function(function) => function.object.clone(),
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
    let mut computed_key_index = 0;
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
                let (private_id, key) =
                    element_key_with(agent, name, strict, precomputed_keys, computed_key_index)?;
                let closure = instantiate_method(agent, function, class_env.clone(), true)?;
                set_private_environment(agent, &closure, &class_private_env)?;
                make_method(agent, &closure, home.clone())?;
                set_function_name(&closure, &element_name_text(name, key.as_ref()), None)?;
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
                let (private_id, key) =
                    element_key_with(agent, name, strict, precomputed_keys, computed_key_index)?;
                let getter =
                    instantiate_accessor(agent, Vec::new(), body.clone(), class_env.clone(), true)?;
                set_private_environment(agent, &getter, &class_private_env)?;
                make_method(agent, &getter, home.clone())?;
                set_function_name(&getter, &element_name_text(name, key.as_ref()), Some("get"))?;
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
                name,
                param,
                init,
                body,
                ..
            } => {
                let (private_id, key) =
                    element_key_with(agent, name, strict, precomputed_keys, computed_key_index)?;
                let setter = instantiate_accessor(
                    agent,
                    vec![BindingElement {
                        pattern: param.clone(),
                        init: init.clone(),
                        rest: false,
                        span: body.span,
                    }],
                    body.clone(),
                    class_env.clone(),
                    true,
                )?;
                set_private_environment(agent, &setter, &class_private_env)?;
                make_method(agent, &setter, home.clone())?;
                set_function_name(&setter, &element_name_text(name, key.as_ref()), Some("set"))?;
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
                let (private_id, key) =
                    element_key_with(agent, name, strict, precomputed_keys, computed_key_index)?;
                if let Some(name_id) = private_id {
                    let ClassElementName::Private(atom) = name else {
                        unreachable!("private id implies a private name");
                    };
                    if is_static {
                        static_elements.push(StaticElement::Field {
                            key: None,
                            private_name: Some(name_id),
                            name_text: JsString::from_utf8(&format!(
                                "#{}",
                                crux::lookup(*atom).to_string_lossy()
                            )),
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
                        key: Some(key.clone().unwrap()),
                        private_name: None,
                        name_text: crate::expr::property_key_display(key.as_ref().unwrap()),
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
        // The compiled key list (see `compile_class` in ir.rs) holds one
        // entry per computed public element in source order; index it by
        // that position, not the element index, or a static method before a
        // computed one consumes its neighbor's key.
        if has_computed_public_name(element) {
            computed_key_index += 1;
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
        // Cut 33: fields/private methods arrive after registration, so the
        // cached construct-inline verdict (computed with the empty vectors)
        // is stale — a constructor with fields/private methods must not
        // inline.
        data.construct_inline = data.compute_construct_inline();
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
                name_text,
                init,
            } => {
                let value = evaluate_static_field_initializer(
                    agent,
                    init.as_ref(),
                    &ctor,
                    &class_env,
                    &class_private_env,
                )?;
                // DefineField step 7: an anonymous function definition takes
                // the field name.
                if init
                    .as_ref()
                    .is_some_and(crate::function::is_anonymous_function_definition)
                {
                    crate::function::set_function_name(&value, name_text, None)?;
                }
                if let Some(name_id) = private_name {
                    private_field_add(&ctor, *name_id, value)?;
                } else {
                    let Some(obj) = home_object(&ctor) else {
                        return Ok(ctor);
                    };
                    obj.create_data_property_or_throw_key(key.as_ref().unwrap(), value)?;
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
/// initialized (spec 15.7.14 steps 41-44). The `Field` variant carries an
/// `Expr` (which embeds 48-byte `JsString` literals), dwarfing `Block`;
/// splitting the two into separate collections is not worth the churn for a
/// transient evaluation queue.
#[allow(clippy::large_enum_variant)]
enum StaticElement {
    Field {
        key: Option<PropertyKey>,
        private_name: Option<u64>,
        /// The SetFunctionName string for an anonymous function initializer
        /// (DefineField step 7): the property key display or `#name`.
        name_text: JsString,
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
            std::rc::Rc::new(Block {
                stmts: Vec::new(),
                span: crux::Span::new(0, 0),
            }),
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
            let key = property_name_key(agent, name, strict)?;
            Ok((None, Some(key)))
        }
    }
}

/// Element name resolution for the resumable-VM path: a computed public name
/// was already evaluated (the `precomputed_keys` entry), private names are
/// resolved here, and non-computed names are synthesized from the AST.
fn element_key_with(
    agent: &mut Agent,
    name: &ClassElementName,
    strict: bool,
    precomputed_keys: Option<&[Option<PropertyKey>]>,
    index: usize,
) -> Result<(Option<u64>, Option<PropertyKey>), JsError> {
    let Some(precomputed_keys) = precomputed_keys else {
        return element_key(agent, name, strict);
    };
    match name {
        ClassElementName::Private(atom) => {
            let name = crate::context::resolve_private_name(agent, *atom)?;
            Ok((Some(name.id), None))
        }
        // Only a computed public name consumes a slot in `precomputed_keys`
        // (indexed by its position among the computed elements, not the
        // element index); a static name is synthesized from the AST, or its
        // computed expression is evaluated inline when the key is absent
        // (the non-VM path shares this fallback).
        ClassElementName::Property(name @ PropertyName::Computed(_)) => {
            match precomputed_keys.get(index).cloned().flatten() {
                Some(key) => Ok((None, Some(key))),
                None => {
                    let key = property_name_key(agent, name, strict)?;
                    Ok((None, Some(key)))
                }
            }
        }
        ClassElementName::Property(name) => {
            let key = property_name_key(agent, name, strict)?;
            Ok((None, Some(key)))
        }
    }
}

/// The SetFunctionName string of a class element name (spec 10.2.7 step 2):
/// the evaluated property key (a symbol renders `[description]`), or the
/// `#name` description of a private name.
fn element_name_text(name: &ClassElementName, key: Option<&PropertyKey>) -> JsString {
    match (name, key) {
        (ClassElementName::Private(atom), _) => {
            JsString::from_utf8(&format!("#{}", crux::lookup(*atom).to_string_lossy()))
        }
        (ClassElementName::Property(_), Some(key)) => crate::expr::property_key_display(key),
        (ClassElementName::Property(_), None) => JsString::from_utf8(""),
    }
}

/// The private identifier of a class element, if its name is private.
pub(crate) fn private_element_name(element: &ClassElement) -> Option<crux::string::AtomId> {
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
    let ValueKind::Function(function) = function.kind() else {
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

/// Evaluate a static field initializer as a method whose [[HomeObject]] is
/// the constructor (spec 15.7.14 FieldDefinition): `this` = the receiver and
/// `super.prop` resolves against the superclass, including inside arrows the
/// initializer creates. Private names resolve through the class environment.
fn evaluate_static_field_initializer(
    agent: &mut Agent,
    init: Option<&Expr>,
    receiver: &Value,
    class_env: &EnvRef,
    class_private_env: &crux::handle::Handle<crate::context::PrivateEnvironment>,
) -> Result<Value, JsError> {
    let Some(init) = init else {
        return Ok(Value::Undefined);
    };
    let body = Block {
        stmts: vec![syntax::ast::Stmt {
            span: init.span,
            kind: syntax::ast::StmtKind::Return(Some(init.clone())),
        }],
        span: init.span,
    };
    let function = FunctionAst {
        span: init.span,
        name: None,
        params: Vec::new(),
        body,
        is_async: false,
        is_generator: false,
        statement_position: false,
    };
    let closure = instantiate_method(agent, &function, class_env.clone(), true)?;
    set_private_environment(agent, &closure, class_private_env)?;
    make_method(agent, &closure, receiver.clone())?;
    // The synthetic initializer function carries [[ClassFieldInitializerName]]
    // (spec 15.7.10 step 8), so a direct eval in its body applies the "Eval
    // Inside Initializer" early errors (spec 19.2.1.1).
    if let ValueKind::Function(function) = closure.kind()
        && let Some(data) = agent.ecma_functions.get_mut(&function.id())
    {
        data.class_field_initializer = true;
    }
    crate::function::call(agent, &closure, receiver.clone(), &[])
}

/// PrivateFieldAdd (spec 10.2.10) on a receiver (object or constructor).
fn private_field_add(receiver: &Value, name_id: u64, value: Value) -> Result<(), JsError> {
    let Some(obj) = home_object(receiver) else {
        return Ok(());
    };
    // spec 10.2.10 step 1: private fields cannot be added to a
    // non-extensible object (even a freshly-returned super() object).
    if !obj.is_extensible()? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot add private field to a non-extensible object".into(),
        ));
    }
    obj.private_element_add(crux::object::PrivateElement {
        name_id,
        kind: crux::object::PrivateElementKind::Field(value),
    })
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
    // DefineMethodProperty uses DefinePropertyOrThrow (spec 10.2.8 step 7): a
    // failed define — e.g. a static method named `prototype` (non-configurable
    // on the constructor) — throws (static-method-non-configurable-err.js).
    if !obj.define_property_key(
        key,
        &PropertyDescriptor {
            value: Some(closure),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot define the method on the class".into(),
        ));
    }
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
    // DefinePropertyOrThrow (spec 15.4.5): a failed define throws, e.g. a
    // static accessor named `prototype`
    // (getters/setters-non-configurable-err.js).
    if !obj.define_property_key(
        key,
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: getter,
            set: setter,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot define the accessor on the class".into(),
        ));
    }
    Ok(())
}

/// The object part of a method home: the class prototype or the constructor
/// (a function object).
fn home_object(home: &Value) -> Option<Handle<crux::object::JsObject>> {
    if let Some(obj) = home.as_object() {
        Some(obj)
    } else {
        home.as_function().map(|f| f.object.clone())
    }
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
        statement_position: false,
    };
    let closure = instantiate_method(agent, &function, class_env.clone(), true)?;
    set_private_environment(agent, &closure, class_private_env)?;
    // MakeMethod(body, homeObject): the home object is the class constructor,
    // so `super.prop` in a static block resolves against the superclass
    // (spec 15.7.13 step 4).
    make_method(agent, &closure, ctor.clone())?;
    crate::function::call(agent, &closure, ctor.clone(), &[])?;
    Ok(())
}
