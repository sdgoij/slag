//! Templates: host functions and objects (v8::FunctionTemplate /
//! v8::ObjectTemplate), the callback info they receive, and the return-value
//! slot.

use std::cell::RefCell;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeCtor, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::Value;

use super::Isolate;
use super::context::Context;
use super::handle::Local;

/// A host callback (v8::FunctionCallback): receives the call info and sets
/// its result via [`FunctionCallbackInfo::get_return_value`]. Throw by
/// calling [`Isolate::throw_exception`]; a pending exception left by the
/// callback propagates out of the JS call.
pub type FunctionCallback = Box<dyn Fn(&FunctionCallbackInfo)>;

/// The call site a host callback receives: `this`, the argument list,
/// whether the call is a construct, and the return-value slot.
pub struct FunctionCallbackInfo<'a> {
    pub(crate) this: Value,
    pub(crate) args: &'a [Value],
    pub(crate) new_target: Option<Value>,
    pub(crate) isolate: *mut Isolate,
    pub(crate) return_value: RefCell<Option<Value>>,
}

impl FunctionCallbackInfo<'_> {
    /// The `this` value of the call.
    pub fn this(&self) -> Local {
        Local(self.this)
    }

    /// The number of arguments.
    pub fn length(&self) -> usize {
        self.args.len()
    }

    /// The argument at `index`, if present.
    pub fn arg(&self, index: usize) -> Option<Local> {
        self.args.get(index).cloned().map(Local)
    }

    /// An iterator over the arguments.
    pub fn args(&self) -> impl Iterator<Item = Local> + '_ {
        self.args.iter().cloned().map(Local)
    }

    /// Whether the call is a construct (`new`).
    pub fn is_construct_call(&self) -> bool {
        self.new_target.is_some()
    }

    /// The isolate the call runs on.
    pub fn isolate(&self) -> *mut Isolate {
        self.isolate
    }

    /// The host-side return-value slot (v8::FunctionCallbackInfo::GetReturnValue).
    pub fn get_return_value(&self) -> ReturnValue<'_> {
        ReturnValue(&self.return_value)
    }
}

/// The host-side return-value slot (v8::ReturnValue): set the callback's
/// result. An unset slot yields *undefined*.
pub struct ReturnValue<'a>(&'a RefCell<Option<Value>>);

impl ReturnValue<'_> {
    pub fn set(&self, value: Local) {
        *self.0.borrow_mut() = Some(value.into_value());
    }

    pub fn set_undefined(&self) {
        *self.0.borrow_mut() = Some(Value::Undefined);
    }

    pub fn set_null(&self) {
        *self.0.borrow_mut() = Some(Value::Null);
    }

    pub fn set_boolean(&self, value: bool) {
        *self.0.borrow_mut() = Some(Value::Boolean(value));
    }

    pub fn set_number(&self, value: f64) {
        *self.0.borrow_mut() = Some(Value::Number(value));
    }

    pub fn set_string(&self, value: impl Into<String>) {
        let text = value.into();
        *self.0.borrow_mut() = Some(Value::String(Handle::new(JsString::from_utf8(&text))));
    }

    /// The currently set value, if any.
    pub fn get(&self) -> Option<Local> {
        (*self.0.borrow()).map(Local)
    }
}

/// A function template: a host function (v8::FunctionTemplate). The
/// function value is materialized per context with
/// [`get_function`](Self::get_function); calls dispatch to the registered
/// callback, and `new` runs the same callback with a fresh instance.
pub struct FunctionTemplate {
    pub(crate) isolate: *mut Isolate,
    pub(crate) callback: RefCell<Option<Rc<FunctionCallback>>>,
    pub(crate) class_name: RefCell<Option<JsString>>,
    pub(crate) instance_template: RefCell<Option<Rc<ObjectTemplate>>>,
    pub(crate) prototype_template: RefCell<Option<Rc<ObjectTemplate>>>,
}

impl FunctionTemplate {
    pub fn new(isolate: &mut Isolate, callback: FunctionCallback) -> Rc<Self> {
        Rc::new(Self {
            isolate: isolate as *mut Isolate,
            callback: RefCell::new(Some(Rc::new(callback))),
            class_name: RefCell::new(None),
            instance_template: RefCell::new(None),
            prototype_template: RefCell::new(None),
        })
    }

    /// The isolate this template was created on.
    pub fn isolate(&self) -> *mut Isolate {
        self.isolate
    }

    /// The function's `name` (and the `.prototype` object's constructor
    /// name link is left to the host).
    pub fn set_class_name(&self, name: &str) {
        *self.class_name.borrow_mut() = Some(JsString::from_utf8(name));
    }

    /// The template for instances created by `new`, created lazily.
    pub fn instance_template(&self) -> Rc<ObjectTemplate> {
        if let Some(template) = self.instance_template.borrow().clone() {
            return template;
        }
        let template = ObjectTemplate::from_ptr(self.isolate());
        *self.instance_template.borrow_mut() = Some(template.clone());
        template
    }

    /// The template for the constructor's `.prototype` object, created
    /// lazily.
    pub fn prototype_template(&self) -> Rc<ObjectTemplate> {
        if let Some(template) = self.prototype_template.borrow().clone() {
            return template;
        }
        let template = ObjectTemplate::from_ptr(self.isolate());
        *self.prototype_template.borrow_mut() = Some(template.clone());
        template
    }

    /// Materialize the function in `context`'s realm (v8::FunctionTemplate::GetFunction).
    pub fn get_function(self: &Rc<Self>, context: &Context) -> Result<Local, JsError> {
        let realm = context.realm();
        let function_prototype = realm
            .intrinsics
            .get("%Function.prototype%")
            .and_then(|value| crate::context::as_object(&value));
        let object_prototype = realm
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| crate::context::as_object(&value));
        let name = self.class_name.borrow().clone();

        let this = Rc::clone(self);
        let callback = this.callback.borrow().clone();
        let call: NativeFn = Box::new(move |this_value, args| {
            let info = FunctionCallbackInfo {
                this: *this_value,
                args,
                new_target: None,
                isolate: this.isolate,
                return_value: RefCell::new(None),
            };
            match callback {
                Some(ref callback) => run_callback(callback, &info),
                None => Ok(Value::Undefined),
            }
        });

        let this = Rc::clone(self);
        let callback = this.callback.borrow().clone();
        let construct: NativeCtor = Box::new(move |new_target, args| {
            let instance = match prototype_object_of(new_target) {
                Some(prototype) => JsObject::ordinary_object_create(Some(prototype)),
                None => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "host constructor has no .prototype".into(),
                    ));
                }
            };
            if let Some(instance_template) = this.instance_template.borrow().clone() {
                let realm = unsafe {
                    let isolate = &*this.isolate;
                    isolate.agent_ptr().as_ref().unwrap().current_realm()?
                };
                instance_template.apply(&realm, &instance)?;
            }
            let info = FunctionCallbackInfo {
                this: Value::Object(instance),
                args,
                new_target: Some(*new_target),
                isolate: this.isolate,
                return_value: RefCell::new(None),
            };
            let result = match callback {
                Some(ref callback) => run_callback(callback, &info)?,
                None => Value::Undefined,
            };
            if result.is_object() {
                Ok(result)
            } else {
                Ok(Value::Object(instance))
            }
        });

        let function =
            Function::create_builtin(name.clone(), 0, call, Some(construct), function_prototype)?;

        // The constructor's `.prototype`: an ordinary object with
        // %Object.prototype% as its prototype, populated from the
        // prototype template (v8: non-writable, non-configurable).
        let this = Rc::clone(self);
        let prototype_object = JsObject::ordinary_object_create(object_prototype);
        if let Some(prototype_template) = this.prototype_template.borrow().clone() {
            prototype_template.apply(realm, &prototype_object)?;
        }
        function.define_property(
            &JsString::from_utf8("prototype"),
            &PropertyDescriptor {
                value: Some(Value::Object(prototype_object)),
                writable: Some(false),
                enumerable: Some(false),
                configurable: Some(false),
                get: None,
                set: None,
            },
        )?;
        Ok(Local(Value::Function(function)))
    }
}

/// An object template: a set of properties applied to instances
/// (v8::ObjectTemplate).
pub struct ObjectTemplate {
    isolate: *mut Isolate,
    properties: RefCell<Vec<TemplateProperty>>,
}

enum TemplateProperty {
    Data {
        name: JsString,
        value: Value,
    },
    Accessor {
        name: JsString,
        getter: Rc<FunctionCallback>,
        setter: Option<Rc<FunctionCallback>>,
    },
    SubTemplate {
        name: JsString,
        template: Rc<ObjectTemplate>,
    },
}

impl ObjectTemplate {
    pub fn new(isolate: &mut Isolate) -> Rc<Self> {
        Self::from_ptr(isolate as *mut Isolate)
    }

    /// Internal: create a template from an isolate pointer (used when
    /// lazily materializing sub-templates from `&self`-only contexts).
    pub(crate) fn from_ptr(isolate: *mut Isolate) -> Rc<Self> {
        Rc::new(Self {
            isolate,
            properties: RefCell::new(Vec::new()),
        })
    }

    /// Define a data property (v8::ObjectTemplate::Set).
    pub fn set(&self, name: &str, value: Local) {
        self.properties.borrow_mut().push(TemplateProperty::Data {
            name: JsString::from_utf8(name),
            value: value.into_value(),
        });
    }

    /// Define a property whose value is a fresh instance of `template`
    /// (v8::ObjectTemplate::Set with a template).
    pub fn set_template(&self, name: &str, template: &Rc<ObjectTemplate>) {
        self.properties
            .borrow_mut()
            .push(TemplateProperty::SubTemplate {
                name: JsString::from_utf8(name),
                template: Rc::clone(template),
            });
    }

    /// Define an accessor property whose getter/setter are host callbacks
    /// (v8::ObjectTemplate::SetAccessor). Accessors receive the object as
    /// `this` and use the return-value slot.
    pub fn set_accessor(
        &self,
        name: &str,
        getter: FunctionCallback,
        setter: Option<FunctionCallback>,
    ) {
        self.properties
            .borrow_mut()
            .push(TemplateProperty::Accessor {
                name: JsString::from_utf8(name),
                getter: Rc::new(getter),
                setter: setter.map(Rc::new),
            });
    }

    /// Create an instance in `context`'s realm (v8::ObjectTemplate::NewInstance).
    pub fn new_instance(&self, context: &Context) -> Result<Local, JsError> {
        let object_prototype = context
            .realm()
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| value.as_object());
        let object = JsObject::ordinary_object_create(object_prototype);
        self.apply(context.realm(), &object)?;
        Ok(Local(Value::Object(object)))
    }

    /// Apply this template's properties onto `target` (an instance or the
    /// constructor's `.prototype` object). Accessor functions are
    /// materialized fresh per application; the getter/setter `Function`
    /// identity therefore differs between instances (divergence from V8,
    /// which shares them).
    pub(crate) fn apply(
        &self,
        realm: &Handle<crate::realm::Realm>,
        target: &Handle<JsObject>,
    ) -> Result<(), JsError> {
        let function_prototype = realm
            .intrinsics
            .get("%Function.prototype%")
            .and_then(|value| crate::context::as_object(&value));
        for property in self.properties.borrow().iter() {
            match property {
                TemplateProperty::Data { name, value } => {
                    target.define_property_or_throw(
                        name,
                        &PropertyDescriptor {
                            value: Some(*value),
                            writable: Some(true),
                            enumerable: Some(true),
                            configurable: Some(true),
                            get: None,
                            set: None,
                        },
                    )?;
                }
                TemplateProperty::Accessor {
                    name,
                    getter,
                    setter,
                } => {
                    let get = host_function(
                        self.isolate,
                        Rc::clone(getter),
                        Some(JsString::from_utf8(&format!(
                            "get {}",
                            name.to_string_lossy()
                        ))),
                        function_prototype,
                    )?;
                    let set = match setter {
                        Some(setter) => Some(
                            host_function(
                                self.isolate,
                                Rc::clone(setter),
                                Some(JsString::from_utf8(&format!(
                                    "set {}",
                                    name.to_string_lossy()
                                ))),
                                function_prototype,
                            )?
                            .self_value(),
                        ),
                        None => None,
                    };
                    target.define_property_or_throw(
                        name,
                        &PropertyDescriptor {
                            value: None,
                            writable: None,
                            get: Some(Value::Function(get)),
                            set,
                            enumerable: Some(true),
                            configurable: Some(true),
                        },
                    )?;
                }
                TemplateProperty::SubTemplate { name, template } => {
                    let instance = template.new_instance_with_realm(realm)?;
                    target.define_property_or_throw(
                        name,
                        &PropertyDescriptor {
                            value: Some(instance),
                            writable: Some(true),
                            enumerable: Some(true),
                            configurable: Some(true),
                            get: None,
                            set: None,
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Create an instance with a realm already in hand (used when applying a
    /// sub-template during another template's application).
    fn new_instance_with_realm(
        &self,
        realm: &Handle<crate::realm::Realm>,
    ) -> Result<Value, JsError> {
        let object_prototype = realm
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| value.as_object());
        let object = JsObject::ordinary_object_create(object_prototype);
        self.apply(realm, &object)?;
        Ok(Value::Object(object))
    }
}

/// Run a host callback, translating a pending exception left by the
/// callback into an engine error so the throw propagates through the JS
/// call. The result is the return-value slot, or *undefined* when unset.
fn run_callback(
    callback: &FunctionCallback,
    info: &FunctionCallbackInfo,
) -> Result<Value, JsError> {
    callback(info);
    let isolate = unsafe { &*info.isolate };
    if let Some(exception) = isolate.take_pending_exception() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "exception thrown by host callback".into(),
        )
        .with_value(exception));
    }
    Ok(info
        .return_value
        .borrow_mut()
        .take()
        .unwrap_or(Value::Undefined))
}

/// Build a bare host function with the given callback and prototype.
fn host_function(
    isolate: *mut Isolate,
    callback: Rc<FunctionCallback>,
    name: Option<JsString>,
    prototype: Option<Handle<JsObject>>,
) -> Result<Handle<Function>, JsError> {
    let call: NativeFn = Box::new(move |this, args| {
        let info = FunctionCallbackInfo {
            this: *this,
            args,
            new_target: None,
            isolate,
            return_value: RefCell::new(None),
        };
        run_callback(&callback, &info)
    });
    Function::create_builtin(name, 0, call, None, prototype)
}

/// The `.prototype` property of the `newTarget` — the instance's prototype.
fn prototype_object_of(new_target: &Value) -> Option<Handle<JsObject>> {
    let function = new_target.as_function()?;
    let value = function.get(&JsString::from_utf8("prototype")).ok()?;
    value.as_object()
}
