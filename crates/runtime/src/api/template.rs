//! Templates: host functions and objects (v8::FunctionTemplate /
//! v8::ObjectTemplate), the callback info they receive, and the return-value
//! slot.

use std::cell::Cell;
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
    pub fn get_return_value(&self) -> ReturnSlot<'_> {
        ReturnSlot(&self.return_value)
    }
}

/// The host-side return-value slot (v8::ReturnValue): set the callback's
/// result. An unset slot yields *undefined*.
///
/// A slot rather than a call view, because two kinds of callback have one: a
/// function call, where [`FunctionCallbackInfo::get_return_value`] hands it out,
/// and a property operation, where the host keeps the storage and reads it back
/// to answer the engine — see [`ReturnSlot::new`].
#[derive(Clone, Copy)]
pub struct ReturnSlot<'a>(&'a RefCell<Option<Value>>);

impl<'a> ReturnSlot<'a> {
    /// A slot over storage the caller owns.
    pub fn new(storage: &'a RefCell<Option<Value>>) -> Self {
        Self(storage)
    }
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
    /// The `length` of the function this template makes, which is an own
    /// property of the function and therefore observable.
    length: Cell<u32>,
    /// Whether the function this template makes can be constructed. A
    /// non-constructible one has no [[Construct]], so `new` on it throws.
    constructible: Cell<bool>,
    /// Properties on the function itself (v8::Template::Set with a
    /// function template), applied when it is materialized.
    static_properties: RefCell<Vec<TemplateProperty>>,
    /// The template this one inherits from (v8::FunctionTemplate::Inherit),
    /// whose `.prototype` becomes this one's prototype's prototype.
    parent: RefCell<Option<Rc<FunctionTemplate>>>,
    /// What this template has already made, one entry per realm.
    ///
    /// The crate materializes a function once per context and hands the same
    /// one back, and two things need that here: a caller that asks twice must
    /// not get two functions, and `Inherit` must find the parent's own
    /// `.prototype` rather than a second copy of it — otherwise
    /// `Child.prototype instanceof Parent` would be false with both objects
    /// looking identical.
    materialized: RefCell<Vec<Materialized>>,
}

/// A function as materialized in one realm.
struct Materialized {
    /// The realm's identity, which is its handle's address. Stable while this
    /// entry lives, because the entry pins the realm it names.
    realm: usize,
    function: Value,
    /// Dropped with the entry, and the only thing keeping the value above — and
    /// the `.prototype` object it carries — alive: nothing else here is traced.
    _pins: Vec<crux::heap::Pin>,
}

impl FunctionTemplate {
    pub fn new(isolate: &mut Isolate, callback: FunctionCallback) -> Rc<Self> {
        Rc::new(Self {
            isolate: isolate as *mut Isolate,
            callback: RefCell::new(Some(Rc::new(callback))),
            class_name: RefCell::new(None),
            instance_template: RefCell::new(None),
            prototype_template: RefCell::new(None),
            length: Cell::new(0),
            constructible: Cell::new(true),
            static_properties: RefCell::new(Vec::new()),
            parent: RefCell::new(None),
            materialized: RefCell::new(Vec::new()),
        })
    }

    /// Set the `length` of the function this template makes
    /// (v8::FunctionTemplate::SetLength).
    ///
    /// A negative length has no meaning as a property value and is taken as
    /// zero, which is the default.
    pub fn set_length(&self, length: i32) {
        self.length.set(length.max(0) as u32);
    }

    /// The host callback this template dispatches to, for a caller that needs
    /// the function rather than the template — the bridge's accessor
    /// properties take their getter and setter as function *templates*, and the
    /// call is what a template's callback is.
    pub fn callback(&self) -> Option<Rc<FunctionCallback>> {
        self.callback.borrow().clone()
    }

    /// Define a property on the function itself (v8::Template::Set with a
    /// function template), which is where a host's static methods go.
    pub fn set(&self, name: &str, value: Local, attributes: PropertyAttributes) {
        self.static_properties
            .borrow_mut()
            .push(TemplateProperty::Data {
                name: JsString::from_utf8(name),
                value: value.into_value(),
                attributes,
            });
    }

    /// Inherit from `parent` (v8::FunctionTemplate::Inherit): the function this
    /// template makes gets `prototype.__proto__ === parent.prototype`.
    ///
    /// Applied at materialization, because the parent's `.prototype` only
    /// exists in a realm once the parent has been materialized in it.
    pub fn inherit(&self, parent: &Rc<FunctionTemplate>) {
        *self.parent.borrow_mut() = Some(Rc::clone(parent));
    }

    /// Say whether the function this template makes may be constructed
    /// (`ConstructorBehavior::Allow` there).
    pub fn set_constructible(&self, constructible: bool) {
        self.constructible.set(constructible);
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
        // One function per realm, as the crate has it: a second ask hands back
        // the one the first made, so its `.prototype` stays the object a child
        // template inherited from.
        let realm_key = realm.as_ptr() as usize;
        if let Some(entry) = self
            .materialized
            .borrow()
            .iter()
            .find(|entry| entry.realm == realm_key)
        {
            return Ok(Local(entry.function));
        }
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

        // A template that is not constructible contributes no construct half, so
        // the function it makes has no [[Construct]] and `new` on it throws.
        let construct = if self.constructible.get() {
            Some(construct)
        } else {
            None
        };
        let function = Function::create_builtin(
            name.clone(),
            self.length.get() as u64,
            call,
            construct,
            function_prototype,
        )?;

        // The constructor's `.prototype`: an ordinary object whose prototype is
        // %Object.prototype%, or the parent's `.prototype` when this template
        // inherits one (v8::FunctionTemplate::Inherit) — populated from the
        // prototype template (v8: non-writable, non-configurable).
        let parent_prototype = match self.parent.borrow().clone() {
            Some(parent) => {
                let parent_function = parent.get_function(context)?;
                let value = crate::api::Object::get(context, &parent_function, "prototype")?.0;
                crate::context::as_object(&value)
            }
            None => None,
        };
        let this = Rc::clone(self);
        let prototype_object =
            JsObject::ordinary_object_create(parent_prototype.or(object_prototype));
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

        // The host's static properties: `Template::Set` on a function template
        // puts them on the function object itself. Data properties only — the
        // crate's static *accessor* is absent until a host asks for it, since
        // the accessor shape this engine has is the one on an object template.
        if !self.static_properties.borrow().is_empty() {
            let function_object = crate::context::as_object(&Value::Function(function))
                .expect("api bug: a function value has an object behind it");
            for property in self.static_properties.borrow().iter() {
                if let TemplateProperty::Data {
                    name,
                    value,
                    attributes,
                } = property
                {
                    function_object.define_property_or_throw(
                        name,
                        &PropertyDescriptor {
                            value: Some(*value),
                            writable: Some(attributes.writable()),
                            enumerable: Some(attributes.enumerable()),
                            configurable: Some(attributes.configurable()),
                            get: None,
                            set: None,
                        },
                    )?;
                }
            }
        }

        // Remember what this realm got, and pin it: nothing else here is traced,
        // and the realm's own address is this entry's key — so the realm is
        // pinned too, or a swept realm's slot could be reused by another one and
        // two realms would answer as one.
        self.materialized.borrow_mut().push(Materialized {
            realm: realm_key,
            function: Value::Function(function),
            _pins: vec![
                crux::heap::pin_handle(*realm),
                crux::heap::pin(Value::Function(function)),
            ],
        });
        Ok(Local(Value::Function(function)))
    }
}

/// An object template: a set of properties applied to instances
/// (v8::ObjectTemplate).
pub struct ObjectTemplate {
    isolate: *mut Isolate,
    properties: RefCell<Vec<TemplateProperty>>,
    /// How many internal fields instances were promised. Recorded and reported,
    /// and nothing else: an object here has no internal-field slots (see the
    /// bridge's `set_internal_field_count`, which says the same where a host
    /// would look).
    internal_field_count: std::cell::Cell<usize>,
    /// Host state the embedder attached. The engine never reads it: it is where
    /// a bridge keeps its own view of a template, which then lives exactly as
    /// long as the isolate that owns the template.
    host_state: RefCell<Option<Rc<dyn std::any::Any>>>,
}

enum TemplateProperty {
    Data {
        name: JsString,
        value: Value,
        attributes: PropertyAttributes,
    },
    Accessor {
        name: JsString,
        getter: Rc<FunctionCallback>,
        setter: Option<Rc<FunctionCallback>>,
        attributes: PropertyAttributes,
    },
    SubTemplate {
        name: JsString,
        template: Rc<ObjectTemplate>,
    },
}

/// The attributes a template's properties are created with
/// (v8::PropertyAttribute).
///
/// The defaults are the crate's `None` — writable, enumerable, configurable —
/// and the negative flags are how the crate spells the other three, so this
/// carries the same three decisions under their own names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PropertyAttributes {
    /// `PropertyAttribute::READ_ONLY`: instances get it non-writable.
    pub read_only: bool,
    /// `PropertyAttribute::DONT_ENUM`: it does not show up in a key walk.
    pub dont_enum: bool,
    /// `PropertyAttribute::DONT_DELETE`: deleting it is refused.
    pub dont_delete: bool,
}

impl PropertyAttributes {
    fn writable(self) -> bool {
        !self.read_only
    }

    fn enumerable(self) -> bool {
        !self.dont_enum
    }

    fn configurable(self) -> bool {
        !self.dont_delete
    }
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
            internal_field_count: std::cell::Cell::new(0),
            host_state: RefCell::new(None),
        })
    }

    /// Record how many internal fields instances are promised
    /// (v8::ObjectTemplate::SetInternalFieldCount).
    pub fn set_internal_field_count(&self, count: usize) {
        self.internal_field_count.set(count);
    }

    /// The count recorded above (v8::ObjectTemplate::InternalFieldCount).
    pub fn internal_field_count(&self) -> usize {
        self.internal_field_count.get()
    }

    /// Attach host state to this template (this layer's own accessor; V8's
    /// `ObjectTemplate` has no counterpart, and nothing here reads it back).
    pub fn set_host_state(&self, state: Rc<dyn std::any::Any>) {
        *self.host_state.borrow_mut() = Some(state);
    }

    /// The host state attached above, if any.
    pub fn host_state(&self) -> Option<Rc<dyn std::any::Any>> {
        self.host_state.borrow().clone()
    }

    /// Define a data property (v8::ObjectTemplate::Set).
    pub fn set(&self, name: &str, value: Local) {
        self.set_with_attributes(name, value, PropertyAttributes::default());
    }

    /// Define a data property with attributes (v8::ObjectTemplate::Set's third
    /// argument, which is where a host's `DONT_ENUM | READ_ONLY` goes).
    pub fn set_with_attributes(&self, name: &str, value: Local, attributes: PropertyAttributes) {
        self.properties.borrow_mut().push(TemplateProperty::Data {
            name: JsString::from_utf8(name),
            value: value.into_value(),
            attributes,
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
        self.set_accessor_with_attributes(name, getter, setter, PropertyAttributes::default());
    }

    /// Define an accessor property with attributes
    /// (v8::ObjectTemplate::SetAccessor's attribute argument).
    pub fn set_accessor_with_attributes(
        &self,
        name: &str,
        getter: FunctionCallback,
        setter: Option<FunctionCallback>,
        attributes: PropertyAttributes,
    ) {
        self.set_accessor_rc(name, Rc::new(getter), setter.map(Rc::new), attributes);
    }

    /// The same over callbacks the caller already holds shared, which is the
    /// shape an accessor built from two *function templates* arrives in: their
    /// callback is the template's own, and cloning the `Rc` is the whole
    /// conversion.
    pub fn set_accessor_rc(
        &self,
        name: &str,
        getter: Rc<FunctionCallback>,
        setter: Option<Rc<FunctionCallback>>,
        attributes: PropertyAttributes,
    ) {
        self.properties
            .borrow_mut()
            .push(TemplateProperty::Accessor {
                name: JsString::from_utf8(name),
                getter,
                setter,
                attributes,
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
                TemplateProperty::Data {
                    name,
                    value,
                    attributes,
                } => {
                    target.define_property_or_throw(
                        name,
                        &PropertyDescriptor {
                            value: Some(*value),
                            writable: Some(attributes.writable()),
                            enumerable: Some(attributes.enumerable()),
                            configurable: Some(attributes.configurable()),
                            get: None,
                            set: None,
                        },
                    )?;
                }
                TemplateProperty::Accessor {
                    name,
                    getter,
                    setter,
                    attributes,
                } => {
                    let get = host_function(
                        self.isolate,
                        Rc::clone(getter),
                        Some(JsString::from_utf8(&format!(
                            "get {}",
                            name.to_string_lossy()
                        ))),
                        function_prototype,
                        false,
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
                                false,
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
                            enumerable: Some(attributes.enumerable()),
                            configurable: Some(attributes.configurable()),
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
///
/// The one place a host callback becomes a callable, so a function a template
/// materializes and one a snapshot restores are the same shape: the same call
/// view, the same return-value slot, the same pending-exception translation.
///
/// `constructible` gives the function the `[[Construct]]` a snapshot recorded it
/// with. The half is the *shape* a template's is — the ordinary object
/// `newTarget.prototype` names, the callback run with `is_construct_call()` true,
/// and the instance unless the callback returned an object — and it is built here
/// rather than shared with `FunctionTemplate::get_function` because a template
/// adds its instance properties to that object, which a function a snapshot
/// restores has no template behind to add.
pub(crate) fn host_function(
    isolate: *mut Isolate,
    callback: Rc<FunctionCallback>,
    name: Option<JsString>,
    prototype: Option<Handle<JsObject>>,
    constructible: bool,
) -> Result<Handle<Function>, JsError> {
    let call: NativeFn = {
        let callback = Rc::clone(&callback);
        Box::new(move |this, args| {
            let info = FunctionCallbackInfo {
                this: *this,
                args,
                new_target: None,
                isolate,
                return_value: RefCell::new(None),
            };
            run_callback(&callback, &info)
        })
    };
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
        let info = FunctionCallbackInfo {
            this: Value::Object(instance),
            args,
            new_target: Some(*new_target),
            isolate,
            return_value: RefCell::new(None),
        };
        let result = run_callback(&callback, &info)?;
        if result.is_object() {
            Ok(result)
        } else {
            Ok(Value::Object(instance))
        }
    });
    Function::create_builtin(name, 0, call, constructible.then_some(construct), prototype)
}

/// The `.prototype` property of the `newTarget` — the instance's prototype.
fn prototype_object_of(new_target: &Value) -> Option<Handle<JsObject>> {
    let function = new_target.as_function()?;
    let value = function.get(&JsString::from_utf8("prototype")).ok()?;
    value.as_object()
}
