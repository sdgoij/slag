//! Environment records (spec 9.2) and every abstract method from the
//! ch. 9 tables: declarative, object, function, module, and global records.
//!
//! Records are shared (outer environments are referenced by many inner
//! ones), so `EnvRef` is an `Rc` and mutable state lives behind `RefCell`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::heap::{GcAny, Trace};
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
    PerIteration(PerIterationEnv),
    Object(ObjectEnv),
    Function(FunctionEnv),
    Global(GlobalEnv),
    Module(ModuleEnv),
}

impl EnvRecord {
    /// The record's [[OuterEnv]], or `None` when null.
    pub fn outer(&self) -> Option<EnvRef> {
        match self {
            EnvRecord::Declarative(e) => e.outer,
            EnvRecord::PerIteration(e) => e.outer,
            EnvRecord::Object(e) => e.outer,
            EnvRecord::Function(e) => e.declarative.outer,
            EnvRecord::Global(e) => e.declarative.outer,
            EnvRecord::Module(e) => e.declarative.outer,
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
            EnvRecord::PerIteration(e) => e.is_parameter(name),
            EnvRecord::Function(e) => e.declarative.is_parameter(name),
            _ => false,
        }
    }

    /// Mark a binding as a formal parameter or `arguments` (declarative and
    /// function records only).
    pub fn mark_parameter(&self, name: &JsString) {
        match self {
            EnvRecord::Declarative(e) => e.mark_parameter(name),
            EnvRecord::PerIteration(e) => e.mark_parameter(name),
            EnvRecord::Function(e) => e.declarative.mark_parameter(name),
            _ => {}
        }
    }

    /// spec 9.2.1.4 HasBinding.
    pub fn has_binding(&self, name: &JsString) -> Result<bool, JsError> {
        match self {
            EnvRecord::Declarative(e) => Ok(e.has_binding(name)),
            EnvRecord::PerIteration(e) => Ok(e.has_binding(name)),
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
            EnvRecord::PerIteration(e) => e.create_mutable_binding(name, deletable),
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
            EnvRecord::PerIteration(e) => e.create_immutable_binding(name, strict),
            EnvRecord::Object(e) => e.create_immutable_binding(name, strict),
            EnvRecord::Function(e) => e.declarative.create_immutable_binding(name, strict),
            EnvRecord::Global(e) => e.create_immutable_binding(name, strict),
            EnvRecord::Module(e) => e.declarative.create_immutable_binding(name, strict),
        }
    }

    /// Push a fully-initialized mutable binding (a fresh per-iteration env's
    /// head copy): one step instead of create_mutable_binding +
    /// initialize_binding. The env is freshly created, so no duplicate or
    /// missing-binding error can arise; the Object form is unreachable from
    /// the per-iteration creators (they use `new_declarative_environment`)
    /// and falls back to the two-step form for totality.
    pub fn push_initialized_binding(&self, name: &JsString, value: Value) -> Result<(), JsError> {
        crux::heap::write_barrier(self, value);
        match self {
            EnvRecord::Declarative(e) => {
                e.push_initialized_binding(name, value);
                Ok(())
            }
            EnvRecord::PerIteration(e) => {
                e.push_initialized_binding(name, value);
                Ok(())
            }
            EnvRecord::Function(e) => {
                e.declarative.push_initialized_binding(name, value);
                Ok(())
            }
            EnvRecord::Global(e) => {
                e.declarative.push_initialized_binding(name, value);
                Ok(())
            }
            EnvRecord::Module(e) => {
                e.declarative.push_initialized_binding(name, value);
                Ok(())
            }
            EnvRecord::Object(e) => {
                e.create_mutable_binding(name, false)?;
                e.initialize_binding(name, value)
            }
        }
    }

    /// spec 9.2.1.3 InitializeBinding.
    pub fn initialize_binding(&self, name: &JsString, value: Value) -> Result<(), JsError> {
        crux::heap::write_barrier(self, value);
        match self {
            EnvRecord::Declarative(e) => e.initialize_binding(name, value),
            EnvRecord::PerIteration(e) => e.initialize_binding(name, value),
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
        crux::heap::write_barrier(self, value);
        match self {
            EnvRecord::Declarative(e) => e.set_mutable_binding(name, value, strict),
            EnvRecord::PerIteration(e) => e.set_mutable_binding(name, value, strict),
            EnvRecord::Object(e) => e.set_mutable_binding(name, value, strict),
            EnvRecord::Function(e) => e.declarative.set_mutable_binding(name, value, strict),
            EnvRecord::Global(e) => e.set_mutable_binding(name, value, strict),
            EnvRecord::Module(e) => e.declarative.set_mutable_binding(name, value, strict),
        }
    }

    /// Write a certified body's context slot at `index` — the compile-time
    /// checks already enforce const and TDZ, so no validation here. The
    /// barrier runs at the record level: the slot buffer is one variant of
    /// this enum, so a target derived from the `DeclarativeEnv` payload would
    /// name an interior address, not the box header.
    pub fn set_slot(&self, index: usize, value: Value) {
        crux::heap::write_barrier(self, value);
        match self {
            EnvRecord::Declarative(e) => e.set_slot(index, value),
            EnvRecord::PerIteration(e) => e.set_slot(index, value),
            EnvRecord::Function(e) => e.declarative.set_slot(index, value),
            EnvRecord::Global(e) => e.declarative.set_slot(index, value),
            EnvRecord::Module(e) => e.declarative.set_slot(index, value),
            EnvRecord::Object(_) => {}
        }
    }

    /// spec 9.2.1.6 GetBindingValue.
    pub fn get_binding_value(&self, name: &JsString, strict: bool) -> Result<Value, JsError> {
        match self {
            EnvRecord::Declarative(e) => e.get_binding_value(name, strict),
            EnvRecord::PerIteration(e) => e.get_binding_value(name, strict),
            EnvRecord::Object(e) => e.get_binding_value(name, strict),
            EnvRecord::Function(e) => e.declarative.get_binding_value(name, strict),
            EnvRecord::Global(e) => e.get_binding_value(name, strict),
            EnvRecord::Module(e) => e.declarative.get_binding_value(name, strict),
        }
    }

    /// AddDisposableResource (spec 9.3.1): push a `using` resource onto the
    /// environment's disposal stack.
    pub fn add_disposable_resource(&self, resource: DisposableResource) {
        crux::heap::write_barrier(self, resource.value);
        crux::heap::write_barrier(self, resource.method);
        match self {
            EnvRecord::Declarative(e) => e.add_disposable_resource(resource),
            EnvRecord::Function(e) => e.declarative.add_disposable_resource(resource),
            EnvRecord::Global(e) => e.declarative.add_disposable_resource(resource),
            EnvRecord::Module(e) => e.declarative.add_disposable_resource(resource),
            EnvRecord::Object(_) | EnvRecord::PerIteration(_) => {}
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
            EnvRecord::Object(_) | EnvRecord::PerIteration(_) => false,
        }
    }

    /// Take the [[DisposableResourceStack]] for DisposeResources.
    pub fn drain_disposable_resources(&self) -> Vec<DisposableResource> {
        match self {
            EnvRecord::Declarative(e) => e.drain_disposable_resources(),
            EnvRecord::Function(e) => e.declarative.drain_disposable_resources(),
            EnvRecord::Global(e) => e.declarative.drain_disposable_resources(),
            EnvRecord::Module(e) => e.declarative.drain_disposable_resources(),
            EnvRecord::Object(_) | EnvRecord::PerIteration(_) => Vec::new(),
        }
    }

    /// spec 9.2.1.7 DeleteBinding.
    pub fn delete_binding(&self, name: &JsString) -> Result<bool, JsError> {
        match self {
            EnvRecord::Declarative(e) => e.delete_binding(name),
            EnvRecord::PerIteration(e) => e.delete_binding(name),
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
            // Tasklist 2.3: a certified body that captures `this` for its
            // arrows stores the value in a capture-context binding under the
            // reserved name; an ENV-PATH arrow (one that failed
            // certification — a rest param, a non-certifiable body) created
            // inside it resolves its lexical `this` through that context.
            EnvRecord::Declarative(e) => e.has_captured_this(),
            EnvRecord::Object(_) | EnvRecord::PerIteration(_) => false,
        }
    }

    /// spec 9.2.1.9 GetThisBinding.
    pub fn get_this_binding(&self) -> Result<Value, JsError> {
        match self {
            EnvRecord::Function(e) => e.get_this_binding(),
            EnvRecord::Global(e) => Ok(Value::Object(e.global_this)),
            EnvRecord::Module(_) => Ok(Value::Undefined),
            // The certified body initialized the reserved binding with its
            // `this` value at entry, so it is always set here.
            EnvRecord::Declarative(e) => e.captured_this_value(),
            EnvRecord::Object(_) | EnvRecord::PerIteration(_) => Err(JsError::new(
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
            EnvRecord::Function(e) => Ok(e.new_target.get()),
            _ => Err(JsError::new(
                ErrorKind::ReferenceError,
                "new.target is only valid inside functions".into(),
            )),
        }
    }

    /// BindThisValue (spec 9.2.2.1) for Function Environment Records.
    pub fn bind_this_value(&self, value: Value) -> Result<(), JsError> {
        crux::heap::write_barrier(self, value);
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
    /// resolve through to the target binding. Boxed because it is rare —
    /// inline, its `JsString` target name made every binding 80 bytes, and a
    /// per-iteration copy's binding vector is one entry per loop head.
    pub indirect: Option<Box<(EnvRef, JsString)>>,
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

impl Trace for DisposableResource {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.value.trace(visit);
        self.method.trace(visit);
    }
}

impl Trace for Binding {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        if let Some(value) = &self.value {
            value.trace(visit);
        }
        if let Some(indirect) = &self.indirect {
            indirect.0.trace(visit);
        }
    }
}

impl Trace for DeclarativeEnv {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        if let Some(outer) = &self.outer {
            outer.trace(visit);
        }
        // The cells are RefCells: `RefCell<T>`'s trace skips a cell that is
        // mutably borrowed mid-collection (per-allocation `--gc-stress`) and
        // aborts the sweep instead of panicking. Tracing the whole `bindings`
        // cell also marks the binding-name keys (flat strings, no edges).
        self.bindings.trace(visit);
        // Mirrors `RefCell<T>`'s own trace: a cell borrowed mid-collection
        // (per-allocation `--gc-stress`) aborts the sweep instead of panicking.
        match self.extras.try_borrow() {
            Ok(extras) => {
                if let Some(extras) = &*extras {
                    extras.trace(visit);
                }
            }
            Err(_) => crux::heap::note_aborted_trace(),
        }
    }
}

impl Trace for ObjectEnv {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        if let Some(outer) = &self.outer {
            outer.trace(visit);
        }
        self.binding_object.trace(visit);
    }
}

impl Trace for FunctionEnv {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.declarative.trace(visit);
        self.function_object.trace(visit);
        // `this_value` is a RefCell: `RefCell<T>`'s trace skips a cell that
        // is mutably borrowed mid-collection (per-allocation `--gc-stress`)
        // and aborts the sweep instead of panicking.
        self.this_value.trace(visit);
        self.new_target.trace(visit);
    }
}

impl Trace for GlobalEnv {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.object.trace(visit);
        self.global_this.trace(visit);
        self.declarative.trace(visit);
    }
}

impl Trace for ModuleEnv {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.declarative.trace(visit);
    }
}

impl Trace for EnvRecord {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        match self {
            EnvRecord::Declarative(e) => e.trace(visit),
            EnvRecord::PerIteration(e) => e.trace(visit),
            EnvRecord::Object(e) => e.trace(visit),
            EnvRecord::Function(e) => e.trace(visit),
            EnvRecord::Global(e) => e.trace(visit),
            EnvRecord::Module(e) => e.trace(visit),
        }
    }
}

/// A Declarative Environment Record (spec 9.2.2), also the base of the
/// Function and Module records.
#[derive(Debug)]
pub struct DeclarativeEnv {
    pub outer: Option<EnvRef>,
    pub bindings: RefCell<Vec<(JsString, Binding)>>,
    /// `using` and Annex B state, allocated on first use. Ordinary scopes —
    /// every block, every per-iteration copy, every function record — carry
    /// neither, and the two vectors were half of the record's footprint; the
    /// record's size is what every environment allocation pays for.
    extras: RefCell<Option<Box<EnvExtras>>>,
    /// The environment of a catch parameter: direct-eval vars may share the
    /// catch parameter's name (Annex B.3.5), so the eval var-vs-lexical walk
    /// skips it.
    pub is_catch_param: std::cell::Cell<bool>,
    /// Transparent to the static context-chain walk: the per-iteration
    /// environments (fresh copies holding only the loop-head names) and the
    /// named-function-expression self-binding scope (instantiation
    /// scaffolding holding only the function's own name). The walk skips
    /// these when resolving an enclosing body's capture context.
    pub context_transparent: std::cell::Cell<bool>,
}

#[derive(Debug, Default)]
struct EnvExtras {
    /// [[DisposableResourceStack]] (spec 9.2.2): populated by `using`
    /// evaluation and drained by DisposeResources at scope exit.
    disposable_resources: Vec<DisposableResource>,
    /// Annex B (B.3.2.1): the block-level FunctionDeclarations this block
    /// hoisted into the variable environment. FunctionDeclaration evaluation
    /// copies the block binding into the var binding for these names.
    annex_b_functions: Vec<JsString>,
}

impl Trace for EnvExtras {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.disposable_resources.trace(visit);
        // `annex_b_functions` holds names only — no edges.
    }
}

impl DeclarativeEnv {
    pub fn new(outer: Option<EnvRef>) -> Self {
        Self {
            outer,
            bindings: RefCell::new(Vec::new()),
            extras: RefCell::new(None),
            is_catch_param: std::cell::Cell::new(false),
            context_transparent: std::cell::Cell::new(false),
        }
    }

    fn with_extras<R>(&self, f: impl FnOnce(&mut EnvExtras) -> R) -> R {
        let mut extras = self.extras.borrow_mut();
        f(extras.get_or_insert_with(Default::default))
    }

    /// Record an Annex B (B.3.2.1) var hoist performed from this block.
    pub fn add_annex_b_function(&self, name: JsString) {
        self.with_extras(|extras| extras.annex_b_functions.push(name));
    }

    pub fn annex_b_hoists(&self, name: &JsString) -> bool {
        self.extras
            .borrow()
            .as_ref()
            .is_some_and(|extras| extras.annex_b_functions.contains(name))
    }

    /// AddDisposableResource (spec 9.3.1): push a `using` resource onto this
    /// environment's disposable-resource stack, drained by DisposeResources
    /// when the scope exits.
    pub fn add_disposable_resource(&self, resource: DisposableResource) {
        self.with_extras(|extras| extras.disposable_resources.push(resource));
    }

    /// Whether the disposable-resource stack is non-empty — the cheap
    /// pre-check before [`Self::drain_disposable_resources`].
    pub fn has_disposable_resources(&self) -> bool {
        self.extras
            .borrow()
            .as_ref()
            .is_some_and(|extras| !extras.disposable_resources.is_empty())
    }

    /// Take the stack for disposal, leaving the environment with none.
    pub fn drain_disposable_resources(&self) -> Vec<DisposableResource> {
        self.extras
            .borrow_mut()
            .as_mut()
            .map(|extras| std::mem::take(&mut extras.disposable_resources))
            .unwrap_or_default()
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
        self.bindings.borrow().get(index).and_then(|(_, b)| b.value)
    }

    /// Whether the binding at `index` is mutable: the static context-chain
    /// store/update steps check this for an *outer* binding (the certified
    /// body's own const writes are compile-rejected, but a nested closure's
    /// write to an enclosing const must still throw).
    pub fn slot_mutable(&self, index: usize) -> bool {
        self.bindings
            .borrow()
            .get(index)
            .is_some_and(|(_, b)| b.mutable)
    }

    /// Mark this environment as transparent to the static context-chain
    /// walk (a loop's per-iteration copy or a named function expression's
    /// self-binding scope — see [`DeclarativeEnv::context_transparent`]).
    pub fn mark_context_transparent(&self) {
        self.context_transparent.set(true);
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

    /// Tasklist 2.3: whether this capture context carries a certified body's
    /// captured `this` (the reserved binding). An env-path arrow's lexical
    /// `this` resolves through it.
    fn has_captured_this(&self) -> bool {
        self.has_binding(&crate::ir::captured_this_name())
    }

    /// The captured-`this` value of a capture context (see
    /// [`DeclarativeEnv::has_captured_this`]); the certified body initialized
    /// the binding at entry, so a missing/undefined state is an internal
    /// error.
    fn captured_this_value(&self) -> Result<Value, JsError> {
        self.get_binding_value(&crate::ir::captured_this_name(), true)
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

    /// Push a fully-initialized mutable binding in ONE step (the per-iteration
    /// env copy). The environment was JUST created empty, so the duplicate
    /// scan (`create_mutable_binding`) and the re-find (`initialize_binding`)
    /// are both redundant — one borrow + push leaves exactly the Binding the
    /// two-step form would. `deletable`/`strict`/`parameter` mirror the
    /// per-iteration creator's arguments.
    pub fn push_initialized_binding(&self, name: &JsString, value: Value) {
        self.bindings.borrow_mut().push((
            name.clone(),
            Binding {
                value: Some(value),
                mutable: true,
                strict: false,
                deletable: false,
                indirect: None,
                parameter: false,
            },
        ));
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

    /// The index of `name` in the binding vector, for the global-value cell's
    /// declarative fast path. The global env bumps its generation on every
    /// binding mutation (create/init/set/delete), so a cached index is only
    /// used while its cell's generation is current; a delete shifts the
    /// vector and therefore invalidates the cell.
    pub fn index_of(&self, name: &JsString) -> Option<usize> {
        self.bindings.borrow().iter().position(|(n, _)| n == name)
    }

    /// Write the binding at a cached index. `None` means the shape moved (the
    /// index now holds another name, or the binding became indirect) and the
    /// caller must resolve; a TDZ or strict-const write returns the same error
    /// the name-keyed path would. `Some(value)` is the binding's value after
    /// the call — the old value when an immutable binding silently ignores a
    /// sloppy write — so the caller mirrors its cell with what is actually
    /// stored.
    pub fn set_value_at(
        &self,
        index: usize,
        name: &JsString,
        value: Value,
        strict: bool,
    ) -> Result<Option<Value>, JsError> {
        let mut bindings = self.bindings.borrow_mut();
        let Some((stored_name, binding)) = bindings.get_mut(index) else {
            return Ok(None);
        };
        if stored_name != name || binding.indirect.is_some() {
            return Ok(None);
        }
        let strict = strict || binding.strict;
        let Some(current) = binding.value else {
            return Err(JsError::new(
                ErrorKind::ReferenceError,
                format!(
                    "Cannot access {:?} before initialization",
                    name.to_string_lossy()
                ),
            ));
        };
        if binding.mutable {
            binding.value = Some(value);
            Ok(Some(value))
        } else {
            if strict {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    format!(
                        "Assignment to constant variable {:?}",
                        name.to_string_lossy()
                    ),
                ));
            }
            Ok(Some(current))
        }
    }

    fn get_binding_value(&self, name: &JsString, _strict: bool) -> Result<Value, JsError> {
        let bindings = self.bindings.borrow();
        let binding = bindings.iter().find(|(n, _)| n == name).ok_or_else(|| {
            JsError::new(
                ErrorKind::ReferenceError,
                format!("{:?} is not defined", name.to_string_lossy()),
            )
        })?;
        if let Some(indirect) = &binding.1.indirect {
            return indirect.0.get_binding_value(&indirect.1, true);
        }
        match &binding.1.value {
            Some(v) => Ok(*v),
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
                indirect: Some(Box::new((target, target_name.clone()))),
                parameter: false,
            },
        ));
        Ok(())
    }
}

/// How many loop-head bindings a per-iteration record holds inline. A head
/// with more declarations keeps the surplus in `extra`.
const INLINE_PER_ITERATION_SLOTS: usize = 2;

/// Where one per-iteration binding lives.
enum SlotLoc {
    /// Slot `0..INLINE_PER_ITERATION_SLOTS` of the head values.
    Inline(usize),
    /// Position in the `extra` table (the surplus heads, plus any binding a
    /// declaration later adds).
    Extra(usize),
}

/// A per-iteration environment record (CreatePerIterationEnvironment, spec
/// 14.7.4.3): one loop iteration's copies of the head bindings.
///
/// The per-iteration copy only ever creates initialized, mutable,
/// non-deletable bindings, so this record stores the values alone, inline in
/// head order, and shares the names with the loop's other iterations. A copy
/// therefore allocates nothing beyond the record itself — the point of the
/// type, since a `for (let …)` body that captures its head makes one per
/// iteration. Anything that does not fit that shape (more heads than the
/// inline slots, a sloppy assignment creating a name) lives in `extra`, an
/// ordinary name-addressed table with the declarative record's semantics.
#[derive(Debug)]
pub struct PerIterationEnv {
    pub outer: Option<EnvRef>,
    names: Rc<[JsString]>,
    values: [Cell<Option<Value>>; INLINE_PER_ITERATION_SLOTS],
    extra: RefCell<Vec<(JsString, Binding)>>,
}

/// The `Binding` the per-iteration copy creates (see
/// [`DeclarativeEnv::push_initialized_binding`], whose shape this mirrors).
fn per_iteration_binding(value: Value) -> Binding {
    Binding {
        value: Some(value),
        mutable: true,
        strict: false,
        deletable: false,
        indirect: None,
        parameter: false,
    }
}

fn binding_not_defined(name: &JsString) -> JsError {
    JsError::new(
        ErrorKind::ReferenceError,
        format!("{:?} is not defined", name.to_string_lossy()),
    )
}

fn binding_uninitialized(name: &JsString) -> JsError {
    JsError::new(
        ErrorKind::ReferenceError,
        format!(
            "Cannot access {:?} before initialization",
            name.to_string_lossy()
        ),
    )
}

fn binding_is_constant(name: &JsString) -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        format!(
            "Assignment to constant variable {:?}",
            name.to_string_lossy()
        ),
    )
}

impl Trace for PerIterationEnv {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        if let Some(outer) = &self.outer {
            outer.trace(visit);
        }
        for name in self.names.iter() {
            name.trace(visit);
        }
        for value in &self.values {
            if let Some(value) = value.get() {
                value.trace(visit);
            }
        }
        // As `RefCell<T>`'s own trace: a cell borrowed mid-collection
        // (per-allocation `--gc-stress`) aborts the sweep instead of panicking.
        match self.extra.try_borrow() {
            Ok(extra) => extra.trace(visit),
            Err(_) => crux::heap::note_aborted_trace(),
        }
    }
}

impl PerIterationEnv {
    fn inline_len(&self) -> usize {
        self.names.len().min(INLINE_PER_ITERATION_SLOTS)
    }

    /// The loop-entry copy: `names` are the step's head list (shared from now
    /// on) and the values come from `source`, addressed by name (the
    /// certified body's capture context, or the loop environment on the
    /// bytecode path).
    fn from_named(
        outer: Option<EnvRef>,
        names: &[JsString],
        source: &EnvRef,
    ) -> Result<Self, JsError> {
        let names: Rc<[JsString]> = Rc::from(names);
        let values = [const { Cell::new(None) }; INLINE_PER_ITERATION_SLOTS];
        let mut extra = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let value = source.get_binding_value(name, false)?;
            if index < INLINE_PER_ITERATION_SLOTS {
                values[index].set(Some(value));
            } else {
                extra.push((name.clone(), per_iteration_binding(value)));
            }
        }
        Ok(Self {
            outer,
            names,
            values,
            extra: RefCell::new(extra),
        })
    }

    /// The per-iteration copy: the values come across by slot and the names
    /// are shared, so nothing is looked up by name and nothing is allocated.
    fn copy_of(outer: Option<EnvRef>, previous: &Self) -> Self {
        let values = [const { Cell::new(None) }; INLINE_PER_ITERATION_SLOTS];
        for (slot, previous) in values.iter().zip(previous.values.iter()) {
            slot.set(previous.get());
        }
        Self {
            outer,
            names: previous.names.clone(),
            values,
            extra: RefCell::new(previous.extra.borrow().clone()),
        }
    }

    /// Whether this record holds exactly `names` in order — the guard on the
    /// slot copy, so a chain of copies can never shift a binding.
    fn names_match(&self, names: &[JsString]) -> bool {
        self.names.len() == names.len()
            && self
                .names
                .iter()
                .zip(names)
                .all(|(stored, name)| stored == name)
    }

    fn locate(&self, name: &JsString) -> Option<SlotLoc> {
        for (index, stored) in self.names.iter().enumerate() {
            if stored == name {
                return Some(if index < INLINE_PER_ITERATION_SLOTS {
                    SlotLoc::Inline(index)
                } else {
                    SlotLoc::Extra(index - INLINE_PER_ITERATION_SLOTS)
                });
            }
        }
        self.extra
            .borrow()
            .iter()
            .position(|(stored, _)| stored == name)
            .map(SlotLoc::Extra)
    }

    /// The location of the binding at binding-order `index`, or `None` when
    /// the index is past the last binding.
    fn slot_loc(&self, index: usize) -> Option<SlotLoc> {
        let inline = self.inline_len();
        if index < inline {
            return Some(SlotLoc::Inline(index));
        }
        let position = index - inline;
        (position < self.extra.borrow().len()).then_some(SlotLoc::Extra(position))
    }

    pub(crate) fn slot_value(&self, index: usize) -> Option<Value> {
        match self.slot_loc(index)? {
            SlotLoc::Inline(slot) => self.values[slot].get(),
            SlotLoc::Extra(position) => self.extra.borrow()[position].1.value,
        }
    }

    pub(crate) fn set_slot(&self, index: usize, value: Value) {
        match self.slot_loc(index) {
            Some(SlotLoc::Inline(slot)) => self.values[slot].set(Some(value)),
            Some(SlotLoc::Extra(position)) => {
                if let Some((_, binding)) = self.extra.borrow_mut().get_mut(position) {
                    binding.value = Some(value);
                }
            }
            None => {}
        }
    }

    pub(crate) fn slot_mutable(&self, index: usize) -> bool {
        match self.slot_loc(index) {
            Some(SlotLoc::Inline(_)) => true,
            Some(SlotLoc::Extra(position)) => self.extra.borrow()[position].1.mutable,
            None => false,
        }
    }

    fn has_binding(&self, name: &JsString) -> bool {
        self.locate(name).is_some()
    }

    fn get_binding_value(&self, name: &JsString, _strict: bool) -> Result<Value, JsError> {
        match self.locate(name) {
            Some(SlotLoc::Inline(slot)) => self.values[slot]
                .get()
                .ok_or_else(|| binding_uninitialized(name)),
            Some(SlotLoc::Extra(position)) => {
                let extra = self.extra.borrow();
                let (_, binding) = &extra[position];
                if let Some(indirect) = &binding.indirect {
                    return indirect.0.get_binding_value(&indirect.1, true);
                }
                binding.value.ok_or_else(|| binding_uninitialized(name))
            }
            None => Err(binding_not_defined(name)),
        }
    }

    fn set_mutable_binding(
        &self,
        name: &JsString,
        value: Value,
        strict: bool,
    ) -> Result<(), JsError> {
        match self.locate(name) {
            Some(SlotLoc::Inline(slot)) => {
                if self.values[slot].get().is_none() {
                    return Err(binding_uninitialized(name));
                }
                self.values[slot].set(Some(value));
                Ok(())
            }
            Some(SlotLoc::Extra(position)) => {
                let mut extra = self.extra.borrow_mut();
                let (_, binding) = &mut extra[position];
                // spec step 2: a strict binding forces strict semantics.
                let strict = strict || binding.strict;
                if binding.value.is_none() {
                    return Err(binding_uninitialized(name));
                }
                if binding.mutable {
                    binding.value = Some(value);
                    Ok(())
                } else if strict {
                    Err(binding_is_constant(name))
                } else {
                    Ok(())
                }
            }
            // spec step 1: sloppy code creates the missing binding.
            None if strict => Err(binding_not_defined(name)),
            None => {
                self.create_mutable_binding(name, true)?;
                self.initialize_binding(name, value)
            }
        }
    }

    fn initialize_binding(&self, name: &JsString, value: Value) -> Result<(), JsError> {
        match self.locate(name) {
            Some(SlotLoc::Inline(slot)) => {
                self.values[slot].set(Some(value));
                Ok(())
            }
            Some(SlotLoc::Extra(position)) => {
                self.extra.borrow_mut()[position].1.value = Some(value);
                Ok(())
            }
            None => Err(JsError::new(
                ErrorKind::ReferenceError,
                format!("Binding {:?} does not exist", name.to_string_lossy()),
            )),
        }
    }

    fn push_extra(&self, name: &JsString, binding: Binding) -> Result<(), JsError> {
        if self.has_binding(name) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("Binding {:?} already exists", name.to_string_lossy()),
            ));
        }
        self.extra.borrow_mut().push((name.clone(), binding));
        Ok(())
    }

    fn create_mutable_binding(&self, name: &JsString, deletable: bool) -> Result<(), JsError> {
        self.push_extra(
            name,
            Binding {
                value: None,
                mutable: true,
                strict: false,
                deletable,
                indirect: None,
                parameter: false,
            },
        )
    }

    fn create_immutable_binding(&self, name: &JsString, strict: bool) -> Result<(), JsError> {
        self.push_extra(
            name,
            Binding {
                value: None,
                mutable: false,
                strict,
                deletable: false,
                indirect: None,
                parameter: false,
            },
        )
    }

    fn push_initialized_binding(&self, name: &JsString, value: Value) {
        if self.has_binding(name) {
            return;
        }
        self.extra
            .borrow_mut()
            .push((name.clone(), per_iteration_binding(value)));
    }

    fn delete_binding(&self, name: &JsString) -> Result<bool, JsError> {
        match self.locate(name) {
            // A loop head is a lexical binding: never deletable.
            Some(SlotLoc::Inline(_)) => Ok(false),
            Some(SlotLoc::Extra(position)) => {
                let mut extra = self.extra.borrow_mut();
                if !extra[position].1.deletable {
                    return Ok(false);
                }
                extra.remove(position);
                Ok(true)
            }
            None => Ok(true),
        }
    }

    fn mark_parameter(&self, name: &JsString) {
        if let Some(SlotLoc::Extra(position)) = self.locate(name)
            && let Some((_, binding)) = self.extra.borrow_mut().get_mut(position)
        {
            binding.parameter = true;
        }
    }

    fn is_parameter(&self, name: &JsString) -> bool {
        match self.locate(name) {
            Some(SlotLoc::Extra(position)) => self.extra.borrow()[position].1.parameter,
            _ => false,
        }
    }

    /// This record's bindings in slot order, flattened into the ordinary
    /// name-addressed shape (what the snapshot writer carries).
    pub fn carried_bindings(&self) -> Vec<(JsString, Binding)> {
        let inline = self.inline_len();
        let extra = self.extra.borrow();
        let mut bindings = Vec::with_capacity(self.names.len() + extra.len());
        for (index, name) in self.names.iter().enumerate() {
            let value = if index < inline {
                self.values[index].get()
            } else {
                extra[index - inline].1.value
            };
            let mut binding = per_iteration_binding(Value::Undefined);
            binding.value = value;
            bindings.push((name.clone(), binding));
        }
        bindings.extend(extra.iter().skip(self.names.len() - inline).cloned());
        bindings
    }
}

/// CreatePerIterationEnvironment (spec 14.7.4.3): a fresh record holding the
/// loop-head bindings copied from the environment they currently live in.
///
/// When `source` is the loop's own previous per-iteration record (the ordinary
/// case inside a loop) the values copy across by slot and the names are
/// shared; otherwise they are looked up by name, which pays for itself once at
/// loop entry.
pub fn new_per_iteration_environment(
    outer: Option<EnvRef>,
    names: &[JsString],
    source: &EnvRef,
) -> Result<EnvRef, JsError> {
    let env = match &**source {
        EnvRecord::PerIteration(previous) if previous.names_match(names) => {
            PerIterationEnv::copy_of(outer, previous)
        }
        _ => PerIterationEnv::from_named(outer, names, source)?,
    };
    Ok(Handle::new(EnvRecord::PerIteration(env)))
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
        let unscopables_key = PropertyKey::Symbol(crux::symbol::unscopables());
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
            Value::Object(self.binding_object)
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
    /// `[[FunctionObject]]`: the function this invocation is running, which is
    /// what `super` resolves the home object through. A cell because a snapshot
    /// reader fills it once the function that owns the record exists.
    pub function_object: Cell<Value>,
    pub this_value: RefCell<Value>,
    pub this_binding_status: Cell<ThisBindingStatus>,
    /// `[[NewTarget]]`: what `new.target` answers. Also filled by a reader.
    pub new_target: Cell<Value>,
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
            function_object: Cell::new(function_object),
            this_value: RefCell::new(Value::Undefined),
            this_binding_status: Cell::new(status),
            new_target: Cell::new(new_target),
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
        Ok(*self.this_value.borrow())
    }

    fn has_super_binding(&self, agent: &Agent) -> bool {
        self.this_binding_status.get() != ThisBindingStatus::Lexical
            && self.function_object_has_home_object(agent)
    }

    fn function_object_has_home_object(&self, agent: &Agent) -> bool {
        // spec 9.2.1.10: `[[FunctionObject]].[[HomeObject]]` is not
        // *undefined* — the function was defined as a method.
        let ValueKind::Function(function) = self.function_object.get().kind() else {
            return false;
        };
        agent
            .ecma_function(&function.id())
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

    /// A declarative binding change alters what a name resolves to without
    /// touching the global object, and the JIT's global-value cell validates a
    /// cached read against the object's generation alone — so every mutation of
    /// the declarative record bumps it.
    fn bump_declarative_generation(&self) {
        self.object.bump_generation();
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
        self.declarative.create_mutable_binding(name, deletable)?;
        self.bump_declarative_generation();
        Ok(())
    }

    fn create_immutable_binding(&self, name: &JsString, strict: bool) -> Result<(), JsError> {
        if self.declarative.has_binding(name) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("Binding {:?} already exists", name.to_string_lossy()),
            ));
        }
        self.declarative.create_immutable_binding(name, strict)?;
        self.bump_declarative_generation();
        Ok(())
    }

    fn initialize_binding(&self, name: &JsString, value: Value) -> Result<(), JsError> {
        if self.declarative.has_binding(name) {
            self.declarative.initialize_binding(name, value)?;
            self.bump_declarative_generation();
            return Ok(());
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
            self.declarative.set_mutable_binding(name, value, strict)?;
            self.bump_declarative_generation();
            return Ok(());
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
            let deleted = self.declarative.delete_binding(name)?;
            self.bump_declarative_generation();
            return Ok(deleted);
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
                value: Some(value),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(deletable),
            },
            Some(prop) if prop.configurable => PropertyDescriptor {
                value: Some(value),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(deletable),
            },
            Some(_) => PropertyDescriptor {
                value: Some(value),
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

    #[test]
    fn global_declarative_changes_bump_the_global_object_generation() {
        // The JIT's global-value cell validates a cached read against the global
        // object's generation alone, and a declarative binding changes what a
        // name resolves to without touching the object — so every declarative
        // mutation has to move the generation, or a warmed cell would keep
        // serving the pre-change value.
        let object = JsObject::ordinary_object_create(None);
        let global = GlobalEnv::new(object, object);
        let mut generation = object.generation();
        global.create_mutable_binding(&name("px"), true).unwrap();
        assert_ne!(object.generation(), generation, "creating bumps it");
        generation = object.generation();
        global
            .initialize_binding(&name("px"), Value::Number(1.0))
            .unwrap();
        assert_ne!(object.generation(), generation, "initializing bumps it");
        generation = object.generation();
        global
            .set_mutable_binding(&name("px"), Value::Number(2.0), false)
            .unwrap();
        assert_ne!(object.generation(), generation, "a write bumps it");
        generation = object.generation();
        assert!(global.delete_binding(&name("px")).unwrap());
        assert_ne!(object.generation(), generation, "deleting bumps it");
        // An OBJECT-record binding still bumps through its own property write.
        generation = object.generation();
        global
            .initialize_binding(&name("py"), Value::Number(3.0))
            .unwrap();
        assert_ne!(
            object.generation(),
            generation,
            "an object binding bumps it"
        );
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
        let env = new_object_environment(obj, true, None);
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
        let key = PropertyKey::Symbol(crux::symbol::unscopables());
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
        let env = new_global_environment(global, global);
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
        let env = new_global_environment(global, global);
        let fun = Value::Function(crux::Function::new(None));
        env.create_global_function_binding(&name("f"), fun, false)
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
        let env = new_global_environment(global, global);
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
        let env = new_global_environment(global, global);
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
    fn declared_binding_value_at_a_cached_index() {
        // The `StoreIdent` declarative fast path: the index is stable while
        // the generation is, a moved index falls back, and const semantics
        // match the name-keyed path.
        let global = JsObject::ordinary_object_create(None);
        let env = new_global_environment(global, global);
        env.create_mutable_binding(&name("m"), false).unwrap();
        env.initialize_binding(&name("m"), Value::Number(1.0))
            .unwrap();
        let EnvRecord::Global(global_env) = &*env else {
            panic!("a global environment")
        };
        let index = global_env
            .declarative
            .index_of(&name("m"))
            .expect("a binding index");
        assert_eq!(
            global_env
                .declarative
                .set_value_at(index, &name("m"), Value::Number(2.0), false)
                .unwrap(),
            Some(Value::Number(2.0))
        );
        assert_eq!(
            env.get_binding_value(&name("m"), true).unwrap(),
            Value::Number(2.0)
        );
        assert_eq!(
            global_env
                .declarative
                .set_value_at(index + 1, &name("m"), Value::Number(3.0), false)
                .unwrap(),
            None,
            "a moved index falls back"
        );

        env.create_immutable_binding(&name("c"), false).unwrap();
        env.initialize_binding(&name("c"), Value::Number(4.0))
            .unwrap();
        let c_index = global_env
            .declarative
            .index_of(&name("c"))
            .expect("a binding index");
        assert_eq!(
            global_env
                .declarative
                .set_value_at(c_index, &name("c"), Value::Number(5.0), false)
                .unwrap(),
            Some(Value::Number(4.0)),
            "a sloppy const write is ignored and reports the stored value"
        );
        assert!(
            global_env
                .declarative
                .set_value_at(c_index, &name("c"), Value::Number(5.0), true)
                .is_err(),
            "a strict const write throws"
        );
    }

    #[test]
    fn redeclared_global_var_binding_is_a_noop() {
        // `var a; var a;` — the second declaration reuses the existing
        // object property instead of failing (spec 9.2.6.10).
        let global = JsObject::ordinary_object_create(None);
        let env = new_global_environment(global, global);
        env.create_global_var_binding(&name("a"), false).unwrap();
        assert!(global.has_own_property(&name("a")).unwrap());
        env.create_global_var_binding(&name("a"), false).unwrap();
        assert!(global.has_own_property(&name("a")).unwrap());
        assert_eq!(global.own_property_keys().unwrap().len(), 1);
    }

    #[test]
    fn per_iteration_record_fits_the_existing_arena_box() {
        // The inline slots remove an allocation per iteration only if they do
        // not grow the box every other environment record allocates: the
        // per-iteration variant has to stay under the largest one.
        assert!(std::mem::size_of::<PerIterationEnv>() <= std::mem::size_of::<FunctionEnv>());
    }

    /// A loop environment holding `names`, initialized to 1, 2, 3…
    fn loop_env(names: &[JsString]) -> EnvRef {
        let env = new_declarative_environment(None);
        for (index, name) in names.iter().enumerate() {
            env.create_mutable_binding(name, false).unwrap();
            env.initialize_binding(name, Value::Number(index as f64 + 1.0))
                .unwrap();
        }
        env
    }

    #[test]
    fn per_iteration_copy_keeps_each_iteration_independent() {
        // The per-iteration record stores the head values inline (and the
        // surplus, here `k`, in its overflow table), so a copy by slot must
        // not alias the record it copied from.
        let names = [name("i"), name("j"), name("k")];
        let loop_env = loop_env(&names);
        let first = new_per_iteration_environment(Some(loop_env), &names, &loop_env).unwrap();
        let EnvRecord::PerIteration(first_record) = &*first else {
            panic!("a per-iteration environment")
        };
        assert_eq!(
            first
                .get_binding_value(&name("k"), false)
                .unwrap()
                .as_number(),
            Some(3.0)
        );
        assert_eq!(
            first_record.slot_value(0).and_then(|v| v.as_number()),
            Some(1.0)
        );
        assert_eq!(
            first_record.slot_value(2).and_then(|v| v.as_number()),
            Some(3.0)
        );
        assert!(first_record.slot_mutable(0) && first_record.slot_mutable(2));
        assert!(!first.delete_binding(&name("i")).unwrap());
        assert!(!first.has_binding(&name("missing")).unwrap());

        let second = new_per_iteration_environment(Some(loop_env), &names, &first).unwrap();
        second
            .set_mutable_binding(&name("j"), Value::Number(20.0), false)
            .unwrap();
        second.set_slot(2, Value::Number(30.0));
        assert_eq!(
            first
                .get_binding_value(&name("j"), false)
                .unwrap()
                .as_number(),
            Some(2.0)
        );
        assert_eq!(
            first
                .get_binding_value(&name("k"), false)
                .unwrap()
                .as_number(),
            Some(3.0)
        );
        assert_eq!(
            second
                .get_binding_value(&name("j"), false)
                .unwrap()
                .as_number(),
            Some(20.0)
        );
        assert_eq!(
            second
                .get_binding_value(&name("k"), false)
                .unwrap()
                .as_number(),
            Some(30.0)
        );
    }

    #[test]
    fn per_iteration_copy_from_a_different_head_falls_back_to_names() {
        // A `PerIteration` step whose source is not this loop's previous
        // record must copy by name: the slot copy is guarded on the names
        // matching, or the bindings would shift.
        let inner = [name("i"), name("j")];
        let outer_names = [name("j"), name("x")];
        let loop_env = loop_env(&outer_names);
        let previous =
            new_per_iteration_environment(Some(loop_env), &outer_names, &loop_env).unwrap();
        let next = new_per_iteration_environment(Some(loop_env), &inner, &previous).unwrap_err();
        assert!(
            next.to_string().contains("i"),
            "the name-addressed copy reports the missing head: {next}"
        );

        // A different head order would read the wrong slot: `x` is slot 1 of
        // `previous`, so a slot copy would produce `j`'s value.
        let reordered = [name("x")];
        let next = new_per_iteration_environment(Some(loop_env), &reordered, &previous).unwrap();
        assert_eq!(
            next.get_binding_value(&name("x"), false)
                .unwrap()
                .as_number(),
            Some(2.0)
        );
    }
}
