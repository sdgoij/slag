//! Environment records (spec 9.2) and every abstract method from the
//! ch. 9 tables: declarative, object, function, module, and global records.
//!
//! Records are shared (outer environments are referenced by many inner
//! ones), so `EnvRef` is an `Rc` and mutable state lives behind `RefCell`.

use std::cell::{Cell, RefCell};

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;

/// A shared reference to an Environment Record.
pub type EnvRef = Handle<EnvRecord>;

/// The Environment Record type hierarchy (spec 9.2.1).
#[derive(Debug)]
pub enum EnvRecord {
    Declarative(DeclarativeEnv),
    Object(ObjectEnv),
    Function(FunctionEnv),
    Global(GlobalEnv),
    Module(ModuleEnv),
}

impl EnvRecord {
    /// The record's [[OuterEnv]], or `None` when null.
    pub fn outer(&self) -> Option<EnvRef> {
        match self {
            EnvRecord::Declarative(e) => e.outer.clone(),
            EnvRecord::Object(e) => e.outer.clone(),
            EnvRecord::Function(e) => e.declarative.outer.clone(),
            EnvRecord::Global(e) => e.declarative.outer.clone(),
            EnvRecord::Module(e) => e.declarative.outer.clone(),
        }
    }

    /// Annex B (B.3.2.1): record a block-level function hoisted from this
    /// block into the variable environment. No-op for non-declarative envs.
    pub fn add_annex_b_function(&self, name: JsString) {
        if let EnvRecord::Declarative(e) = self {
            e.add_annex_b_function(name);
        }
    }

    /// Mark this declarative environment as a catch parameter's scope.
    pub fn mark_catch_param_env(&self) {
        if let EnvRecord::Declarative(e) = self {
            e.is_catch_param.set(true);
        }
    }

    pub fn is_catch_param_env(&self) -> bool {
        matches!(self, EnvRecord::Declarative(e) if e.is_catch_param.get())
    }

    /// True when the binding is a formal parameter or `arguments` (the Annex
    /// B block-function hoist must not overwrite it).
    pub fn is_parameter_binding(&self, name: &JsString) -> bool {
        match self {
            EnvRecord::Declarative(e) => e.is_parameter(name),
            EnvRecord::Function(e) => e.declarative.is_parameter(name),
            _ => false,
        }
    }

    /// Mark a binding as a formal parameter or `arguments` (declarative and
    /// function records only).
    pub fn mark_parameter(&self, name: &JsString) {
        match self {
            EnvRecord::Declarative(e) => e.mark_parameter(name),
            EnvRecord::Function(e) => e.declarative.mark_parameter(name),
            _ => {}
        }
    }

    /// spec 9.2.1.4 HasBinding.
    pub fn has_binding(&self, name: &JsString) -> Result<bool, JsError> {
        match self {
            EnvRecord::Declarative(e) => Ok(e.has_binding(name)),
            EnvRecord::Object(e) => e.has_binding(name),
            EnvRecord::Function(e) => Ok(e.declarative.has_binding(name)),
            EnvRecord::Global(e) => e.has_binding(name),
            EnvRecord::Module(e) => Ok(e.declarative.has_binding(name)),
        }
    }

    /// spec 9.2.1.1 CreateMutableBinding.
    pub fn create_mutable_binding(&self, name: &JsString, deletable: bool) -> Result<(), JsError> {
        match self {
            EnvRecord::Declarative(e) => e.create_mutable_binding(name, deletable),
            EnvRecord::Object(e) => e.create_mutable_binding(name, deletable),
            EnvRecord::Function(e) => e.declarative.create_mutable_binding(name, deletable),
            EnvRecord::Global(e) => e.create_mutable_binding(name, deletable),
            EnvRecord::Module(e) => e.declarative.create_mutable_binding(name, deletable),
        }
    }

    /// spec 9.2.1.2 CreateImmutableBinding.
    pub fn create_immutable_binding(&self, name: &JsString, strict: bool) -> Result<(), JsError> {
        match self {
            EnvRecord::Declarative(e) => e.create_immutable_binding(name, strict),
            EnvRecord::Object(e) => e.create_immutable_binding(name, strict),
            EnvRecord::Function(e) => e.declarative.create_immutable_binding(name, strict),
            EnvRecord::Global(e) => e.create_immutable_binding(name, strict),
            EnvRecord::Module(e) => e.declarative.create_immutable_binding(name, strict),
        }
    }

    /// spec 9.2.1.3 InitializeBinding.
    pub fn initialize_binding(&self, name: &JsString, value: Value) -> Result<(), JsError> {
        match self {
            EnvRecord::Declarative(e) => e.initialize_binding(name, value),
            EnvRecord::Object(e) => e.initialize_binding(name, value),
            EnvRecord::Function(e) => e.declarative.initialize_binding(name, value),
            EnvRecord::Global(e) => e.initialize_binding(name, value),
            EnvRecord::Module(e) => e.declarative.initialize_binding(name, value),
        }
    }

    /// spec 9.2.1.5 SetMutableBinding.
    pub fn set_mutable_binding(
        &self,
        name: &JsString,
        value: Value,
        strict: bool,
    ) -> Result<(), JsError> {
        match self {
            EnvRecord::Declarative(e) => e.set_mutable_binding(name, value, strict),
            EnvRecord::Object(e) => e.set_mutable_binding(name, value, strict),
            EnvRecord::Function(e) => e.declarative.set_mutable_binding(name, value, strict),
            EnvRecord::Global(e) => e.set_mutable_binding(name, value, strict),
            EnvRecord::Module(e) => e.declarative.set_mutable_binding(name, value, strict),
        }
    }

    /// spec 9.2.1.6 GetBindingValue.
    pub fn get_binding_value(&self, name: &JsString, strict: bool) -> Result<Value, JsError> {
        match self {
            EnvRecord::Declarative(e) => e.get_binding_value(name, strict),
            EnvRecord::Object(e) => e.get_binding_value(name, strict),
            EnvRecord::Function(e) => e.declarative.get_binding_value(name, strict),
            EnvRecord::Global(e) => e.get_binding_value(name, strict),
            EnvRecord::Module(e) => e.declarative.get_binding_value(name, strict),
        }
    }

    /// AddDisposableResource (spec 9.3.1): push a `using` resource onto the
    /// environment's disposal stack.
    pub fn add_disposable_resource(&self, resource: DisposableResource) {
        match self {
            EnvRecord::Declarative(e) => e.add_disposable_resource(resource),
            EnvRecord::Function(e) => e.declarative.add_disposable_resource(resource),
            EnvRecord::Global(e) => e.declarative.add_disposable_resource(resource),
            EnvRecord::Module(e) => e.declarative.add_disposable_resource(resource),
            EnvRecord::Object(_) => {}
        }
    }

    /// Whether the env has a non-empty [[DisposableResourceStack]] — the
    /// cheap pre-check before [`Self::drain_disposable_resources`].
    pub fn has_disposable_resources(&self) -> bool {
        match self {
            EnvRecord::Declarative(e) => e.has_disposable_resources(),
            EnvRecord::Function(e) => e.declarative.has_disposable_resources(),
            EnvRecord::Global(e) => e.declarative.has_disposable_resources(),
            EnvRecord::Module(e) => e.declarative.has_disposable_resources(),
            EnvRecord::Object(_) => false,
        }
    }

    /// Take the [[DisposableResourceStack]] for DisposeResources.
    pub fn drain_disposable_resources(&self) -> Vec<DisposableResource> {
        match self {
            EnvRecord::Declarative(e) => e.drain_disposable_resources(),
            EnvRecord::Function(e) => e.declarative.drain_disposable_resources(),
            EnvRecord::Global(e) => e.declarative.drain_disposable_resources(),
            EnvRecord::Module(e) => e.declarative.drain_disposable_resources(),
            EnvRecord::Object(_) => Vec::new(),
        }
    }

    /// spec 9.2.1.7 DeleteBinding.
    pub fn delete_binding(&self, name: &JsString) -> Result<bool, JsError> {
        match self {
            EnvRecord::Declarative(e) => e.delete_binding(name),
            EnvRecord::Object(e) => e.delete_binding(name),
            EnvRecord::Function(e) => e.declarative.delete_binding(name),
            EnvRecord::Global(e) => e.delete_binding(name),
            EnvRecord::Module(e) => e.declarative.delete_binding(name),
        }
    }

    /// spec 9.2.1.8 HasThisBinding.
    pub fn has_this_binding(&self) -> bool {
        match self {
            EnvRecord::Function(e) => e.has_this_binding(),
            EnvRecord::Global(_) | EnvRecord::Module(_) => true,
            EnvRecord::Declarative(_) | EnvRecord::Object(_) => false,
        }
    }

    /// spec 9.2.1.9 GetThisBinding.
    pub fn get_this_binding(&self) -> Result<Value, JsError> {
        match self {
            EnvRecord::Function(e) => e.get_this_binding(),
            EnvRecord::Global(e) => Ok(Value::Object(e.global_this.clone())),
            EnvRecord::Module(_) => Ok(Value::Undefined),
            _ => Err(JsError::new(
                ErrorKind::ReferenceError,
                "No this binding in this environment".into(),
            )),
        }
    }

    /// spec 9.2.1.10 HasSuperBinding.
    pub fn has_super_binding(&self, agent: &Agent) -> bool {
        match self {
            EnvRecord::Function(e) => e.has_super_binding(agent),
            _ => false,
        }
    }

    /// spec 9.2.1.11 WithBaseObject.
    pub fn with_base_object(&self) -> Value {
        match self {
            EnvRecord::Object(e) => e.with_base_object(),
            _ => Value::Undefined,
        }
    }

    /// GetNewTarget (spec 9.4.3): the [[NewTarget]] of a Function Environment
    /// Record; an error elsewhere (only function bodies may use new.target).
    pub fn get_new_target(&self) -> Result<Value, JsError> {
        match self {
            EnvRecord::Function(e) => Ok(e.new_target.clone()),
            _ => Err(JsError::new(
                ErrorKind::ReferenceError,
                "new.target is only valid inside functions".into(),
            )),
        }
    }

    /// BindThisValue (spec 9.2.2.1) for Function Environment Records.
    pub fn bind_this_value(&self, value: Value) -> Result<(), JsError> {
        match self {
            EnvRecord::Function(e) => e.bind_this_value(value),
            _ => Err(JsError::new(
                ErrorKind::ReferenceError,
                "No this binding in this environment".into(),
            )),
        }
    }

    /// Global-environment operations (spec 9.2.6); only meaningful on a
    /// Global Environment Record.
    pub fn has_lexical_declaration(&self, name: &JsString) -> bool {
        match self {
            EnvRecord::Global(e) => e.declarative.has_binding(name),
            _ => false,
        }
    }

    pub fn has_restricted_global_property(&self, name: &JsString) -> Result<bool, JsError> {
        match self {
            EnvRecord::Global(e) => e.has_restricted_global_property(name),
            _ => Ok(false),
        }
    }

    pub fn can_declare_global_var(&self, name: &JsString) -> Result<bool, JsError> {
        match self {
            EnvRecord::Global(e) => e.can_declare_global_var(name),
            _ => Ok(true),
        }
    }

    pub fn can_declare_global_function(&self, name: &JsString) -> Result<bool, JsError> {
        match self {
            EnvRecord::Global(e) => e.can_declare_global_function(name),
            _ => Ok(true),
        }
    }

    pub fn create_global_var_binding(
        &self,
        name: &JsString,
        deletable: bool,
    ) -> Result<(), JsError> {
        match self {
            EnvRecord::Global(e) => e.create_global_var_binding(name, deletable),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "Not a global environment record".into(),
            )),
        }
    }

    pub fn create_global_function_binding(
        &self,
        name: &JsString,
        value: Value,
        deletable: bool,
    ) -> Result<(), JsError> {
        match self {
            EnvRecord::Global(e) => e.create_global_function_binding(name, value, deletable),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "Not a global environment record".into(),
            )),
        }
    }
}

/// One binding held by a declarative-style record.
#[derive(Debug, Clone)]
pub struct Binding {
    /// `None` while the binding is uninitialized (TDZ).
    pub value: Option<Value>,
    pub mutable: bool,
    /// A strict binding: writes always throw, whatever the caller's mode.
    pub strict: bool,
    pub deletable: bool,
    /// Module import indirection (spec 9.2.5 CreateImportBinding): reads
    /// resolve through to the target binding.
    pub indirect: Option<(EnvRef, JsString)>,
    /// A formal parameter or `arguments` binding: the Annex B block-function
    /// hoist (B.3.2.1) must not overwrite it.
    pub parameter: bool,
}

/// One `using` declaration's resource (spec 9.3.1): the value and the
/// dispose method captured when the declaration was evaluated.
/// Whether a `using` resource's dispose method returns a promise that must
/// be awaited (spec 9.3.1: the `await using` hint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisposalHint {
    Sync,
    Async,
}

/// A `using` resource on an environment's disposal stack (spec 9.3.1).
#[derive(Debug, Clone)]
pub struct DisposableResource {
    pub value: Value,
    pub method: Value,
    pub hint: DisposalHint,
}

/// A Declarative Environment Record (spec 9.2.2), also the base of the
/// Function and Module records.
#[derive(Debug)]
pub struct DeclarativeEnv {
    pub outer: Option<EnvRef>,
    pub bindings: RefCell<Vec<(JsString, Binding)>>,
    /// [[DisposableResourceStack]] (spec 9.2.2): populated by `using`
    /// evaluation and drained by DisposeResources at scope exit.
    pub disposable_resources: RefCell<Vec<DisposableResource>>,
    /// Annex B (B.3.2.1): the block-level FunctionDeclarations this block
    /// hoisted into the variable environment. FunctionDeclaration evaluation
    /// copies the block binding into the var binding for these names.
    pub annex_b_functions: RefCell<Vec<JsString>>,
    /// The environment of a catch parameter: direct-eval vars may share the
    /// catch parameter's name (Annex B.3.5), so the eval var-vs-lexical walk
    /// skips it.
    pub is_catch_param: std::cell::Cell<bool>,
}

impl DeclarativeEnv {
    pub fn new(outer: Option<EnvRef>) -> Self {
        Self {
            outer,
            bindings: RefCell::new(Vec::new()),
            disposable_resources: RefCell::new(Vec::new()),
            annex_b_functions: RefCell::new(Vec::new()),
            is_catch_param: std::cell::Cell::new(false),
        }
    }

    /// Record an Annex B (B.3.2.1) var hoist performed from this block.
    pub fn add_annex_b_function(&self, name: JsString) {
        self.annex_b_functions.borrow_mut().push(name);
    }

    pub fn annex_b_hoists(&self, name: &JsString) -> bool {
        self.annex_b_functions.borrow().contains(name)
    }

    /// AddDisposableResource (spec 9.3.1): push a `using` resource onto this
    /// environment's disposable-resource stack, drained by DisposeResources
    /// when the scope exits.
    pub fn add_disposable_resource(&self, resource: DisposableResource) {
        self.disposable_resources.borrow_mut().push(resource);
    }

    /// Whether the disposable-resource stack is non-empty — the cheap
    /// pre-check before [`Self::drain_disposable_resources`].
    pub fn has_disposable_resources(&self) -> bool {
        !self.disposable_resources.borrow().is_empty()
    }

    /// Take the stack for disposal, leaving the environment with none.
    pub fn drain_disposable_resources(&self) -> Vec<DisposableResource> {
        std::mem::take(&mut *self.disposable_resources.borrow_mut())
    }

    /// Mark a binding as a formal parameter or `arguments` (the Annex B
    /// block-function hoist must not overwrite it).
    pub fn mark_parameter(&self, name: &JsString) {
        if let Some((_, binding)) = self
            .bindings
            .borrow_mut()
            .iter_mut()
            .find(|(n, _)| n == name)
        {
            binding.parameter = true;
        }
    }

    fn is_parameter(&self, name: &JsString) -> bool {
        self.bindings
            .borrow()
            .iter()
            .any(|(n, b)| n == name && b.parameter)
    }

    /// The value of the binding at `index` (the certified body's capture
    /// context): `None` is the TDZ marker (a `let`/`const` binding that has
    /// not been initialized).
    pub fn slot_value(&self, index: usize) -> Option<Value> {
        self.bindings
            .borrow()
            .get(index)
            .and_then(|(_, b)| b.value.clone())
    }

    /// Write the binding at `index` — the certified body's compile-time
    /// checks already enforce const and TDZ, so no validation here.
    pub fn set_slot(&self, index: usize, value: Value) {
        if let Some((_, b)) = self.bindings.borrow_mut().get_mut(index) {
            b.value = Some(value);
        }
    }

    fn has_binding(&self, name: &JsString) -> bool {
        self.bindings.borrow().iter().any(|(n, _)| n == name)
    }

    fn create_mutable_binding(&self, name: &JsString, deletable: bool) -> Result<(), JsError> {
        if self.has_binding(name) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("Binding {:?} already exists", name.to_string_lossy()),
            ));
        }
        self.bindings.borrow_mut().push((
            name.clone(),
            Binding {
                value: None,
                mutable: true,
                strict: false,
                deletable,
                indirect: None,
                parameter: false,
            },
        ));
        Ok(())
    }

    fn create_immutable_binding(&self, name: &JsString, strict: bool) -> Result<(), JsError> {
        if self.has_binding(name) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("Binding {:?} already exists", name.to_string_lossy()),
            ));
        }
        self.bindings.borrow_mut().push((
            name.clone(),
            Binding {
                value: None,
                mutable: false,
                strict,
                deletable: false,
                indirect: None,
                parameter: false,
            },
        ));
        Ok(())
    }

    fn initialize_binding(&self, name: &JsString, value: Value) -> Result<(), JsError> {
        let mut bindings = self.bindings.borrow_mut();
        let binding = bindings
            .iter_mut()
            .find(|(n, _)| n == name)
            .ok_or_else(|| {
                JsError::new(
                    ErrorKind::ReferenceError,
                    format!("Binding {:?} does not exist", name.to_string_lossy()),
                )
            })?;
        binding.1.value = Some(value);
        Ok(())
    }

    fn set_mutable_binding(
        &self,
        name: &JsString,
        value: Value,
        strict: bool,
    ) -> Result<(), JsError> {
        let Some(index) = self.bindings.borrow().iter().position(|(n, _)| n == name) else {
            // spec step 1: sloppy code creates the missing binding.
            if strict {
                return Err(JsError::new(
                    ErrorKind::ReferenceError,
                    format!("{:?} is not defined", name.to_string_lossy()),
                ));
            }
            self.create_mutable_binding(name, true)?;
            return self.initialize_binding(name, value);
        };
        let mut bindings = self.bindings.borrow_mut();
        let binding = &mut bindings[index].1;
        // spec step 2: a strict binding forces strict semantics.
        let strict = strict || binding.strict;
        if binding.value.is_none() {
            return Err(JsError::new(
                ErrorKind::ReferenceError,
                format!(
                    "Cannot access {:?} before initialization",
                    name.to_string_lossy()
                ),
            ));
        }
        if binding.mutable {
            binding.value = Some(value);
        } else if strict {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!(
                    "Assignment to constant variable {:?}",
                    name.to_string_lossy()
                ),
            ));
        }
        Ok(())
    }

    fn get_binding_value(&self, name: &JsString, _strict: bool) -> Result<Value, JsError> {
        let bindings = self.bindings.borrow();
        let binding = bindings.iter().find(|(n, _)| n == name).ok_or_else(|| {
            JsError::new(
                ErrorKind::ReferenceError,
                format!("{:?} is not defined", name.to_string_lossy()),
            )
        })?;
        if let Some((target, target_name)) = &binding.1.indirect {
            return target.get_binding_value(target_name, true);
        }
        match &binding.1.value {
            Some(v) => Ok(v.clone()),
            None => Err(JsError::new(
                ErrorKind::ReferenceError,
                format!(
                    "Cannot access {:?} before initialization",
                    name.to_string_lossy()
                ),
            )),
        }
    }

    fn delete_binding(&self, name: &JsString) -> Result<bool, JsError> {
        let mut bindings = self.bindings.borrow_mut();
        let Some(index) = bindings.iter().position(|(n, _)| n == name) else {
            return Ok(true);
        };
        if !bindings[index].1.deletable {
            return Ok(false);
        }
        bindings.remove(index);
        Ok(true)
    }

    /// CreateImportBinding (spec 9.2.5.2): an initialized immutable indirect
    /// binding that reads through to `target_name` in `target`. The value slot
    /// is marked initialized so writes report the immutable TypeError rather
    /// than a TDZ ReferenceError; reads always go through `indirect`.
    fn create_import_binding(
        &self,
        name: &JsString,
        target: EnvRef,
        target_name: &JsString,
    ) -> Result<(), JsError> {
        if self.has_binding(name) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("Binding {:?} already exists", name.to_string_lossy()),
            ));
        }
        self.bindings.borrow_mut().push((
            name.clone(),
            Binding {
                value: Some(Value::Undefined),
                mutable: false,
                strict: true,
                deletable: false,
                indirect: Some((target, target_name.clone())),
                parameter: false,
            },
        ));
        Ok(())
    }
}

/// An Object Environment Record (spec 9.2.3).
#[derive(Debug)]
pub struct ObjectEnv {
    pub outer: Option<EnvRef>,
    pub binding_object: Handle<JsObject>,
    pub is_with: bool,
}

impl ObjectEnv {
    pub fn new(binding_object: Handle<JsObject>, is_with: bool, outer: Option<EnvRef>) -> Self {
        Self {
            outer,
            binding_object,
            is_with,
        }
    }

    fn has_binding(&self, name: &JsString) -> Result<bool, JsError> {
        if !self.binding_object.has_property(name)? {
            return Ok(false);
        }
        if !self.is_with {
            return Ok(true);
        }
        // spec 9.2.3.1 steps 4-9: `with` bindings are blocked when the
        // binding object's %Symbol.unscopables% property maps the name to a
        // truthy value.
        let unscopables_key = PropertyKey::Symbol(crux::symbol::unscopables().as_ref().clone());
        let unscopables = self.binding_object.get_key(&unscopables_key)?;
        if let ValueKind::Object(unscopables_obj) = unscopables.kind()
            && crux::convert::to_boolean(&unscopables_obj.get(name)?)
        {
            return Ok(false);
        }
        Ok(true)
    }

    fn create_mutable_binding(&self, name: &JsString, deletable: bool) -> Result<(), JsError> {
        let desc = PropertyDescriptor {
            value: Some(Value::Undefined),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(true),
            configurable: Some(deletable),
        };
        self.binding_object.define_property_or_throw(name, &desc)
    }

    fn create_immutable_binding(&self, _name: &JsString, _strict: bool) -> Result<(), JsError> {
        // spec: never used within this specification.
        Err(JsError::new(
            ErrorKind::TypeError,
            "Object environment records cannot create immutable bindings".into(),
        ))
    }

    fn initialize_binding(&self, name: &JsString, value: Value) -> Result<(), JsError> {
        self.set_mutable_binding(name, value, false)
    }

    fn set_mutable_binding(
        &self,
        name: &JsString,
        value: Value,
        strict: bool,
    ) -> Result<(), JsError> {
        if !self.binding_object.has_property(name)? && strict {
            return Err(JsError::new(
                ErrorKind::ReferenceError,
                format!("{:?} is not defined", name.to_string_lossy()),
            ));
        }
        self.binding_object.set(name, value, strict)?;
        Ok(())
    }

    fn get_binding_value(&self, name: &JsString, strict: bool) -> Result<Value, JsError> {
        if !self.binding_object.has_property(name)? {
            if strict {
                return Err(JsError::new(
                    ErrorKind::ReferenceError,
                    format!("{:?} is not defined", name.to_string_lossy()),
                ));
            }
            return Ok(Value::Undefined);
        }
        self.binding_object.get(name)
    }

    fn delete_binding(&self, name: &JsString) -> Result<bool, JsError> {
        self.binding_object.delete(name)
    }

    fn with_base_object(&self) -> Value {
        if self.is_with {
            Value::Object(self.binding_object.clone())
        } else {
            Value::Undefined
        }
    }
}

/// The `this` binding state of a Function Environment Record (spec 9.2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThisBindingStatus {
    Lexical,
    Uninitialized,
    Initialized,
}

/// A Function Environment Record (spec 9.2.4): a Declarative record plus the
/// `this`/`super`/`new.target` state of one invocation.
#[derive(Debug)]
pub struct FunctionEnv {
    pub declarative: DeclarativeEnv,
    pub function_object: Value,
    pub this_value: RefCell<Value>,
    pub this_binding_status: Cell<ThisBindingStatus>,
    pub new_target: Value,
}

impl FunctionEnv {
    pub fn new(
        outer: Option<EnvRef>,
        function_object: Value,
        new_target: Value,
        lexical_this: bool,
    ) -> Self {
        let status = if lexical_this {
            ThisBindingStatus::Lexical
        } else {
            ThisBindingStatus::Uninitialized
        };
        Self {
            declarative: DeclarativeEnv::new(outer),
            function_object,
            this_value: RefCell::new(Value::Undefined),
            this_binding_status: Cell::new(status),
            new_target,
        }
    }

    fn has_this_binding(&self) -> bool {
        self.this_binding_status.get() != ThisBindingStatus::Lexical
    }

    fn get_this_binding(&self) -> Result<Value, JsError> {
        if self.this_binding_status.get() == ThisBindingStatus::Uninitialized {
            return Err(JsError::new(
                ErrorKind::ReferenceError,
                "Cannot access 'this' before initialization".into(),
            ));
        }
        Ok(self.this_value.borrow().clone())
    }

    fn has_super_binding(&self, agent: &Agent) -> bool {
        self.this_binding_status.get() != ThisBindingStatus::Lexical
            && self.function_object_has_home_object(agent)
    }

    fn function_object_has_home_object(&self, agent: &Agent) -> bool {
        // spec 9.2.1.10: `[[FunctionObject]].[[HomeObject]]` is not
        // *undefined* — the function was defined as a method.
        let ValueKind::Function(function) = self.function_object.kind() else {
            return false;
        };
        agent
            .ecma_functions
            .get(&function.id())
            .is_some_and(|data| data.home_object.is_some())
    }

    fn bind_this_value(&self, value: Value) -> Result<(), JsError> {
        debug_assert!(self.this_binding_status.get() != ThisBindingStatus::Lexical);
        if self.this_binding_status.get() == ThisBindingStatus::Initialized {
            return Err(JsError::new(
                ErrorKind::ReferenceError,
                "this binding is already initialized".into(),
            ));
        }
        *self.this_value.borrow_mut() = value;
        self.this_binding_status.set(ThisBindingStatus::Initialized);
        Ok(())
    }
}

/// A Global Environment Record (spec 9.2.6): an object record over the
/// global object plus a declarative record for lexical bindings.
#[derive(Debug)]
pub struct GlobalEnv {
    pub object: Handle<JsObject>,
    pub global_this: Handle<JsObject>,
    pub declarative: DeclarativeEnv,
}

impl GlobalEnv {
    pub fn new(object: Handle<JsObject>, global_this: Handle<JsObject>) -> Self {
        Self {
            object,
            global_this,
            declarative: DeclarativeEnv::new(None),
        }
    }

    fn has_binding(&self, name: &JsString) -> Result<bool, JsError> {
        Ok(self.declarative.has_binding(name) || self.object.has_property(name)?)
    }

    fn create_mutable_binding(&self, name: &JsString, deletable: bool) -> Result<(), JsError> {
        if self.declarative.has_binding(name) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("Binding {:?} already exists", name.to_string_lossy()),
            ));
        }
        self.declarative.create_mutable_binding(name, deletable)
    }

    fn create_immutable_binding(&self, name: &JsString, strict: bool) -> Result<(), JsError> {
        if self.declarative.has_binding(name) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("Binding {:?} already exists", name.to_string_lossy()),
            ));
        }
        self.declarative.create_immutable_binding(name, strict)
    }

    fn initialize_binding(&self, name: &JsString, value: Value) -> Result<(), JsError> {
        if self.declarative.has_binding(name) {
            return self.declarative.initialize_binding(name, value);
        }
        self.object.set(name, value, false)?;
        Ok(())
    }

    fn set_mutable_binding(
        &self,
        name: &JsString,
        value: Value,
        strict: bool,
    ) -> Result<(), JsError> {
        if self.declarative.has_binding(name) {
            return self.declarative.set_mutable_binding(name, value, strict);
        }
        // spec 9.2.6.5 steps 2.b-c: a write to a global property that no
        // longer exists (e.g. a getter deleted it) throws a ReferenceError
        // in strict mode instead of recreating the property.
        if !self.object.has_property(name)? && strict {
            return Err(JsError::new(
                ErrorKind::ReferenceError,
                format!("{:?} is not defined", name.to_string_lossy()),
            ));
        }
        self.object.set(name, value, strict)?;
        Ok(())
    }

    fn get_binding_value(&self, name: &JsString, strict: bool) -> Result<Value, JsError> {
        if self.declarative.has_binding(name) {
            return self.declarative.get_binding_value(name, strict);
        }
        // Sloppy mode reads an absent global as *undefined*; strict mode
        // needs to distinguish an actual `undefined` property from a missing
        // binding. [[Get]] includes the prototype chain, so one lookup serves
        // the found case (spec 9.2.6.4).
        let value = self.object.get(name)?;
        if strict && value.is_undefined() && !self.object.has_property(name)? {
            return Err(JsError::new(
                ErrorKind::ReferenceError,
                format!("{:?} is not defined", name.to_string_lossy()),
            ));
        }
        Ok(value)
    }

    fn delete_binding(&self, name: &JsString) -> Result<bool, JsError> {
        if self.declarative.has_binding(name) {
            return self.declarative.delete_binding(name);
        }
        if self.object.has_own_property(name)? {
            return self.object.delete(name);
        }
        Ok(true)
    }

    /// spec 9.2.6.7 HasRestrictedGlobalProperty.
    fn has_restricted_global_property(&self, name: &JsString) -> Result<bool, JsError> {
        match self.object.get_own_property(name)? {
            Some(prop) => Ok(!prop.configurable),
            None => Ok(false),
        }
    }

    /// spec 9.2.6.8 CanDeclareGlobalVar.
    fn can_declare_global_var(&self, name: &JsString) -> Result<bool, JsError> {
        Ok(self.object.has_own_property(name)? || self.object.is_extensible()?)
    }

    /// spec 9.2.6.9 CanDeclareGlobalFunction.
    fn can_declare_global_function(&self, name: &JsString) -> Result<bool, JsError> {
        match self.object.get_own_property(name)? {
            None => Ok(self.object.is_extensible()?),
            Some(prop) => {
                // An existing accessor property blocks the declaration.
                let Some(writable) = prop.writable() else {
                    return Ok(false);
                };
                Ok(prop.configurable || (writable && prop.enumerable))
            }
        }
    }

    /// spec 9.2.6.10 CreateGlobalVarBinding.
    fn create_global_var_binding(&self, name: &JsString, deletable: bool) -> Result<(), JsError> {
        if !self.object.has_own_property(name)? && self.object.is_extensible()? {
            let desc = PropertyDescriptor {
                value: Some(Value::Undefined),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(deletable),
            };
            self.object.define_property_or_throw(name, &desc)?;
        }
        Ok(())
    }

    /// spec 9.2.6.11 CreateGlobalFunctionBinding.
    fn create_global_function_binding(
        &self,
        name: &JsString,
        value: Value,
        deletable: bool,
    ) -> Result<(), JsError> {
        let existing = self.object.get_own_property(name)?;
        let desc = match existing {
            None => PropertyDescriptor {
                value: Some(value.clone()),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(deletable),
            },
            Some(prop) if prop.configurable => PropertyDescriptor {
                value: Some(value.clone()),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(deletable),
            },
            Some(_) => PropertyDescriptor {
                value: Some(value.clone()),
                writable: None,
                get: None,
                set: None,
                enumerable: None,
                configurable: None,
            },
        };
        self.object.define_property_or_throw(name, &desc)?;
        self.object.set(name, value, false)?;
        Ok(())
    }
}

/// A Module Environment Record (spec 9.2.5): a Declarative record with
/// immutable indirect import bindings. Modules themselves are Phase 7.
#[derive(Debug)]
pub struct ModuleEnv {
    pub declarative: DeclarativeEnv,
}

impl ModuleEnv {
    pub fn new(outer: Option<EnvRef>) -> Self {
        Self {
            declarative: DeclarativeEnv::new(outer),
        }
    }
}

/// NewDeclarativeEnvironment (spec 9.2.2.1).
pub fn new_declarative_environment(outer: Option<EnvRef>) -> EnvRef {
    Handle::new(EnvRecord::Declarative(DeclarativeEnv::new(outer)))
}

/// NewObjectEnvironment (spec 9.2.3.1).
pub fn new_object_environment(
    object: Handle<JsObject>,
    is_with: bool,
    outer: Option<EnvRef>,
) -> EnvRef {
    Handle::new(EnvRecord::Object(ObjectEnv::new(object, is_with, outer)))
}

/// NewFunctionEnvironment (spec 9.2.4.1). `lexical_this` marks arrow
/// functions: their environment records have no `this` binding.
pub fn new_function_environment(
    outer: Option<EnvRef>,
    function_object: Value,
    new_target: Value,
    lexical_this: bool,
) -> EnvRef {
    Handle::new(EnvRecord::Function(FunctionEnv::new(
        outer,
        function_object,
        new_target,
        lexical_this,
    )))
}

/// NewGlobalEnvironment (spec 9.2.6.1).
pub fn new_global_environment(object: Handle<JsObject>, this_value: Handle<JsObject>) -> EnvRef {
    Handle::new(EnvRecord::Global(GlobalEnv::new(object, this_value)))
}

/// NewModuleEnvironment (spec 9.2.5.1).
pub fn new_module_environment(outer: Option<EnvRef>) -> EnvRef {
    Handle::new(EnvRecord::Module(ModuleEnv::new(outer)))
}

/// CreateImportBinding on a Module Environment Record (spec 9.2.5.2).
pub fn create_import_binding(
    env: &EnvRef,
    name: &JsString,
    target: EnvRef,
    target_name: &JsString,
) -> Result<(), JsError> {
    match &**env {
        EnvRecord::Module(m) => m
            .declarative
            .create_import_binding(name, target, target_name),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "Not a module environment record".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> JsString {
        JsString::from_utf8(text)
    }

    fn declarative() -> EnvRef {
        new_declarative_environment(None)
    }

    #[test]
    fn declarative_binding_lifecycle() {
        let env = declarative();
        assert!(!env.has_binding(&name("x")).unwrap());
        env.create_mutable_binding(&name("x"), false).unwrap();
        assert!(env.has_binding(&name("x")).unwrap());
        // Uninitialized: TDZ on read.
        assert!(env.get_binding_value(&name("x"), true).is_err());
        env.initialize_binding(&name("x"), Value::Number(1.0))
            .unwrap();
        assert_eq!(
            env.get_binding_value(&name("x"), true).unwrap(),
            Value::Number(1.0)
        );
        env.set_mutable_binding(&name("x"), Value::Number(2.0), true)
            .unwrap();
        assert_eq!(
            env.get_binding_value(&name("x"), true).unwrap(),
            Value::Number(2.0)
        );
    }

    #[test]
    fn immutable_binding_rejects_writes_in_strict() {
        let env = declarative();
        // A strict immutable binding (like `const`) throws in any mode.
        env.create_immutable_binding(&name("c"), true).unwrap();
        env.initialize_binding(&name("c"), Value::Number(1.0))
            .unwrap();
        assert!(
            env.set_mutable_binding(&name("c"), Value::Number(2.0), true)
                .is_err()
        );
        assert!(
            env.set_mutable_binding(&name("c"), Value::Number(2.0), false)
                .is_err()
        );
        assert_eq!(
            env.get_binding_value(&name("c"), true).unwrap(),
            Value::Number(1.0)
        );

        // A non-strict immutable binding is silently ignored in sloppy mode.
        let env = declarative();
        env.create_immutable_binding(&name("d"), false).unwrap();
        env.initialize_binding(&name("d"), Value::Number(1.0))
            .unwrap();
        env.set_mutable_binding(&name("d"), Value::Number(2.0), false)
            .unwrap();
        assert_eq!(
            env.get_binding_value(&name("d"), true).unwrap(),
            Value::Number(1.0)
        );
        assert!(
            env.set_mutable_binding(&name("d"), Value::Number(2.0), true)
                .is_err()
        );
    }

    #[test]
    fn sloppy_set_creates_missing_binding() {
        let env = declarative();
        env.set_mutable_binding(&name("x"), Value::Number(3.0), false)
            .unwrap();
        assert_eq!(
            env.get_binding_value(&name("x"), false).unwrap(),
            Value::Number(3.0)
        );
        // ...but strict mode reports it as undefined.
        let strict_env = declarative();
        assert!(
            strict_env
                .set_mutable_binding(&name("x"), Value::Number(3.0), true)
                .is_err()
        );
    }

    #[test]
    fn deletable_bindings_are_removable() {
        let env = declarative();
        env.create_mutable_binding(&name("a"), true).unwrap();
        env.create_mutable_binding(&name("b"), false).unwrap();
        assert!(env.delete_binding(&name("a")).unwrap());
        assert!(!env.has_binding(&name("a")).unwrap());
        assert!(!env.delete_binding(&name("b")).unwrap());
        assert!(env.has_binding(&name("b")).unwrap());
    }

    #[test]
    fn nested_environments_keep_their_own_bindings() {
        let outer = declarative();
        outer
            .create_mutable_binding(&name("outer_x"), false)
            .unwrap();
        outer
            .initialize_binding(&name("outer_x"), Value::Null)
            .unwrap();
        let inner = new_declarative_environment(Some(outer));
        inner
            .create_mutable_binding(&name("inner_x"), false)
            .unwrap();
        assert!(inner.has_binding(&name("inner_x")).unwrap());
        assert!(!inner.has_binding(&name("outer_x")).unwrap());
        assert!(
            inner
                .outer()
                .unwrap()
                .has_binding(&name("outer_x"))
                .unwrap()
        );
    }

    #[test]
    fn object_environment_binds_object_properties() {
        let obj = JsObject::ordinary_object_create(None);
        obj.create_data_property(&name("p"), Value::Number(5.0))
            .unwrap();
        let env = new_object_environment(obj, false, None);
        assert!(env.has_binding(&name("p")).unwrap());
        assert_eq!(
            env.get_binding_value(&name("p"), true).unwrap(),
            Value::Number(5.0)
        );
        env.set_mutable_binding(&name("p"), Value::Number(6.0), true)
            .unwrap();
        assert_eq!(
            env.get_binding_value(&name("p"), true).unwrap(),
            Value::Number(6.0)
        );
        env.create_mutable_binding(&name("q"), true).unwrap();
        env.initialize_binding(&name("q"), Value::Number(7.0))
            .unwrap();
        assert_eq!(
            env.get_binding_value(&name("q"), true).unwrap(),
            Value::Number(7.0)
        );
    }

    #[test]
    fn with_environment_reports_binding_object() {
        let obj = JsObject::ordinary_object_create(None);
        let env = new_object_environment(obj.clone(), true, None);
        assert_eq!(env.with_base_object(), Value::Object(obj));
        let plain = new_object_environment(JsObject::ordinary_object_create(None), false, None);
        assert_eq!(plain.with_base_object(), Value::Undefined);
    }

    #[test]
    fn with_environment_respects_unscopables() {
        let obj = JsObject::ordinary_object_create(None);
        obj.create_data_property(&name("blocked"), Value::Number(1.0))
            .unwrap();
        obj.create_data_property(&name("visible"), Value::Number(2.0))
            .unwrap();
        // obj[Symbol.unscopables] = { blocked: true }.
        let unscopables = JsObject::ordinary_object_create(None);
        unscopables
            .create_data_property(&name("blocked"), Value::Boolean(true))
            .unwrap();
        let key = PropertyKey::Symbol(crux::symbol::unscopables().as_ref().clone());
        obj.create_data_property_key(&key, Value::Object(unscopables))
            .unwrap();

        let env = new_object_environment(obj, true, None);
        assert!(!env.has_binding(&name("blocked")).unwrap());
        assert!(env.has_binding(&name("visible")).unwrap());
        // A non-`with` environment ignores unscopables entirely.
        let plain_obj = JsObject::ordinary_object_create(None);
        plain_obj
            .create_data_property(&name("blocked"), Value::Number(3.0))
            .unwrap();
        plain_obj
            .create_data_property_key(&key, Value::Boolean(true))
            .unwrap();
        let plain = new_object_environment(plain_obj, false, None);
        assert!(plain.has_binding(&name("blocked")).unwrap());
    }

    #[test]
    fn global_environment_splits_lexical_and_object_bindings() {
        let global = JsObject::ordinary_object_create(None);
        let env = new_global_environment(global.clone(), global.clone());
        assert!(env.has_this_binding());
        assert!(matches!(
            env.get_this_binding().unwrap().kind(),
            ValueKind::Object(_)
        ));
        // Lexical bindings go to the declarative record...
        env.create_mutable_binding(&name("let_x"), false).unwrap();
        env.initialize_binding(&name("let_x"), Value::Number(1.0))
            .unwrap();
        assert!(env.has_lexical_declaration(&name("let_x")));
        // ...and var bindings land on the global object.
        env.create_global_var_binding(&name("var_y"), false)
            .unwrap();
        assert!(!env.has_lexical_declaration(&name("var_y")));
        assert!(global.has_own_property(&name("var_y")).unwrap());
        assert_eq!(
            env.get_binding_value(&name("var_y"), true).unwrap(),
            Value::Undefined
        );
    }

    #[test]
    fn global_function_binding_defines_a_property() {
        let global = JsObject::ordinary_object_create(None);
        let env = new_global_environment(global.clone(), global.clone());
        let fun = Value::Function(crux::Function::new(None));
        env.create_global_function_binding(&name("f"), fun.clone(), false)
            .unwrap();
        assert!(global.has_own_property(&name("f")).unwrap());
        assert_eq!(env.get_binding_value(&name("f"), true).unwrap(), fun);
    }

    #[test]
    fn restricted_global_property_blocks_lexical_shadowing() {
        let global = JsObject::ordinary_object_create(None);
        // Infinity-like: non-configurable own property.
        global
            .define_property(
                &name("Infinity"),
                &crux::PropertyDescriptor::none(Value::Number(f64::INFINITY)),
            )
            .unwrap();
        let env = new_global_environment(global.clone(), global.clone());
        assert!(
            env.has_restricted_global_property(&name("Infinity"))
                .unwrap()
        );
        assert!(!env.has_restricted_global_property(&name("other")).unwrap());
    }

    #[test]
    fn module_environment_reads_through_indirect_bindings() {
        let target = declarative();
        target.create_mutable_binding(&name("real"), false).unwrap();
        target
            .initialize_binding(&name("real"), Value::Number(9.0))
            .unwrap();
        let module = new_module_environment(None);
        create_import_binding(&module, &name("alias"), target, &name("real")).unwrap();
        assert_eq!(
            module.get_binding_value(&name("alias"), true).unwrap(),
            Value::Number(9.0)
        );
        assert!(module.has_this_binding());
        assert_eq!(module.get_this_binding().unwrap(), Value::Undefined);
    }

    // ---- Phase 4 binding semantics: TDZ and global interactions ----

    #[test]
    fn tdz_read_errors_in_sloppy_mode_too() {
        // An uninitialized binding is a TDZ ReferenceError regardless of the
        // strict flag on GetBindingValue/SetMutableBinding (spec 9.2.1.5/6).
        let env = declarative();
        env.create_mutable_binding(&name("x"), false).unwrap();
        let err = env.get_binding_value(&name("x"), false).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
        let err = env
            .set_mutable_binding(&name("x"), Value::Number(1.0), false)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
        // ...and a sloppy write does not initialize the binding.
        assert!(env.get_binding_value(&name("x"), false).is_err());
    }

    #[test]
    fn global_lexical_binding_stays_off_the_object() {
        let global = JsObject::ordinary_object_create(None);
        let env = new_global_environment(global.clone(), global.clone());
        env.create_mutable_binding(&name("let_y"), false).unwrap();
        env.initialize_binding(&name("let_y"), Value::Number(3.0))
            .unwrap();
        assert!(env.has_lexical_declaration(&name("let_y")));
        assert!(!global.has_own_property(&name("let_y")).unwrap());
        assert_eq!(
            env.get_binding_value(&name("let_y"), true).unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn redeclared_global_var_binding_is_a_noop() {
        // `var a; var a;` — the second declaration reuses the existing
        // object property instead of failing (spec 9.2.6.10).
        let global = JsObject::ordinary_object_create(None);
        let env = new_global_environment(global.clone(), global.clone());
        env.create_global_var_binding(&name("a"), false).unwrap();
        assert!(global.has_own_property(&name("a")).unwrap());
        env.create_global_var_binding(&name("a"), false).unwrap();
        assert!(global.has_own_property(&name("a")).unwrap());
        assert_eq!(global.own_property_keys().unwrap().len(), 1);
    }
}
