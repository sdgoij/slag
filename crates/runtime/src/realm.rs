//! Realms (spec 9.3): Realm Records, the intrinsic registry, and the
//! bootstrap pipeline (CreateIntrinsics, NewGlobalEnvironment,
//! SetDefaultGlobalBindings).

use std::cell::RefCell;
use std::collections::HashMap;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::heap::{GcAny, Trace};
use crux::object::JsObject;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::env::{EnvRef, new_global_environment};

/// A Realm Record (spec 9.3 table): the intrinsic registry, the global
/// object, and the global environment.
#[derive(Debug)]
pub struct Realm {
    pub agent_signifier: u64,
    pub intrinsics: Intrinsics,
    pub global_object: Handle<JsObject>,
    pub global_env: EnvRef,
    /// [[LoadedModules]] (spec 9.3): the Source Text Module Records keyed by
    /// resolved specifier.
    pub loaded_modules:
        RefCell<std::collections::HashMap<JsString, Handle<crate::module::SourceTextModule>>>,
}

impl Trace for Realm {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.intrinsics.trace(visit);
        self.global_object.trace(visit);
        self.global_env.trace(visit);
        // `loaded_modules` is a RefCell: `RefCell<T>`'s trace skips a cell
        // that is mutably borrowed mid-collection (per-allocation
        // `--gc-stress`) and aborts the sweep instead of panicking.
        self.loaded_modules.trace(visit);
    }
}

impl Realm {
    pub fn global_env(&self) -> EnvRef {
        self.global_env
    }
}

/// The intrinsic registry (spec 9.3.1): %-named values installed by each
/// built-in phase, in spec bootstrap order.
#[derive(Debug, Default)]
pub struct Intrinsics {
    entries: RefCell<HashMap<JsString, Value>>,
    /// Cut 26: the realm's %Object.prototype% handle, cached after the first
    /// resolution — the intrinsics table is populated at bootstrap and never
    /// reassigned, so the handle is stable for the realm's life. Object
    /// literals (`ObjectBegin`) and constructor `this` fallbacks read it per
    /// object creation.
    object_prototype: RefCell<Option<Value>>,
    /// Cut 66: the function-creation prototype intrinsics (%Function.prototype%
    /// and the generator/async variants), resolved once per realm like
    /// `object_prototype` — `set_function_prototype`/`make_constructor` read
    /// them on every closure creation, and `Intrinsics::get` allocates a
    /// JsString per call. A fixed array indexed by [`function_prototype_index`]
    /// keeps the read a plain borrow + load (no hashing at all).
    function_prototypes: RefCell<[Option<Value>; FN_PROTO_COUNT]>,
    /// The realm's %Function.prototype.apply% / %Function.prototype.call%
    /// builtins, cached after the first resolution: the compiled `CallApply`
    /// step compares the member-read result against these per call, and the
    /// values are stable for the realm's life (the intrinsics table is
    /// populated at bootstrap and never reassigned).
    apply_builtin: RefCell<Option<Value>>,
    call_builtin: RefCell<Option<Value>>,
    /// The realm's %String.prototype% value, cached after the first
    /// resolution: primitive-string member reads resolve their chain
    /// directly against it instead of boxing a per-read String-exotic
    /// wrapper (tasklist 1.4 follow-on). `Intrinsics::get` allocates a
    /// JsString per call, so the cache keeps a hot `s.charAt`-style read off
    /// the allocation path.
    string_prototype: RefCell<Option<Value>>,
    /// The other primitive-creation prototype intrinsics (%Number.prototype%,
    /// %Boolean.prototype%, %BigInt.prototype%, %Symbol.prototype%), cached
    /// after the first resolution like `string_prototype`: non-string
    /// primitive member reads resolve their chain against these directly
    /// (their wrappers are ordinary objects with no own properties, so a
    /// chain read with the primitive receiver is exact).
    primitive_prototypes: RefCell<[Option<Value>; PRIM_PROTO_COUNT]>,
}

/// The number of cached function-creation prototype intrinsics.
pub(crate) const FN_PROTO_COUNT: usize = 6;

/// The number of cached non-string primitive-creation prototype intrinsics
/// (the primitive member-read fast path resolves chain reads against them).
pub(crate) const PRIM_PROTO_COUNT: usize = 4;

/// The slot index for a non-string primitive prototype intrinsic name.
pub(crate) fn primitive_prototype_index(name: &str) -> usize {
    match name {
        "%Number.prototype%" => 0,
        "%Boolean.prototype%" => 1,
        "%BigInt.prototype%" => 2,
        "%Symbol.prototype%" => 3,
        _ => unreachable!("not a cached primitive prototype intrinsic: {name}"),
    }
}

/// The slot index for a function-creation prototype intrinsic name.
pub(crate) fn function_prototype_index(name: &str) -> usize {
    match name {
        "%Function.prototype%" => 0,
        "%GeneratorFunction.prototype%" => 1,
        "%AsyncFunction.prototype%" => 2,
        "%AsyncGeneratorFunction.prototype%" => 3,
        "%Generator.prototype%" => 4,
        "%AsyncGenerator.prototype%" => 5,
        _ => unreachable!("not a cached function-creation intrinsic: {name}"),
    }
}

impl Trace for Intrinsics {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        // The cells are RefCells: `RefCell<T>`'s trace skips a cell that is
        // mutably borrowed mid-collection (per-allocation `--gc-stress`) and
        // aborts the sweep instead of panicking.
        self.entries.trace(visit);
        self.object_prototype.trace(visit);
        match self.function_prototypes.try_borrow() {
            Ok(guard) => {
                for slot in guard.iter() {
                    slot.trace(visit);
                }
            }
            Err(_) => crux::heap::note_aborted_trace(),
        }
        self.apply_builtin.trace(visit);
        self.call_builtin.trace(visit);
        self.string_prototype.trace(visit);
        match self.primitive_prototypes.try_borrow() {
            Ok(guard) => {
                for slot in guard.iter() {
                    slot.trace(visit);
                }
            }
            Err(_) => crux::heap::note_aborted_trace(),
        }
    }
}

impl Intrinsics {
    pub fn get(&self, name: &str) -> Option<Value> {
        self.entries
            .borrow()
            .get(&JsString::from_utf8(name))
            .cloned()
    }

    /// The realm's %Object.prototype% value, cached after the first
    /// resolution (see the struct field).
    pub fn object_prototype(&self) -> Option<Value> {
        if let Some(value) = self.object_prototype.borrow().as_ref() {
            return Some(*value);
        }
        let value = self.get("%Object.prototype%")?;
        *self.object_prototype.borrow_mut() = Some(value);
        Some(value)
    }

    /// A function-creation prototype intrinsic (%Function.prototype%,
    /// %GeneratorFunction.prototype%, %AsyncFunction.prototype%,
    /// %AsyncGeneratorFunction.prototype%, %Generator.prototype%,
    /// %AsyncGenerator.prototype%), cached after the first resolution (see
    /// the struct field).
    pub fn function_prototype(&self, name: &'static str) -> Option<Value> {
        let index = function_prototype_index(name);
        if let Some(value) = self.function_prototypes.borrow()[index] {
            return Some(value);
        }
        let value = self.get(name)?;
        self.function_prototypes.borrow_mut()[index] = Some(value);
        Some(value)
    }

    /// The realm's %Function.prototype.apply% builtin, cached after the first
    /// resolution (see the struct field).
    pub fn apply_builtin(&self) -> Option<Value> {
        if let Some(value) = self.apply_builtin.borrow().as_ref() {
            return Some(*value);
        }
        let value = self.get("%Function.prototype.apply%")?;
        *self.apply_builtin.borrow_mut() = Some(value);
        Some(value)
    }

    /// The realm's %Function.prototype.call% builtin, cached after the first
    /// resolution (see the struct field).
    pub fn call_builtin(&self) -> Option<Value> {
        if let Some(value) = self.call_builtin.borrow().as_ref() {
            return Some(*value);
        }
        let value = self.get("%Function.prototype.call%")?;
        *self.call_builtin.borrow_mut() = Some(value);
        Some(value)
    }

    /// The realm's %String.prototype% value, cached after the first
    /// resolution (see the struct field).
    pub fn string_prototype(&self) -> Option<Value> {
        if let Some(value) = self.string_prototype.borrow().as_ref() {
            return Some(*value);
        }
        let value = self.get("%String.prototype%")?;
        *self.string_prototype.borrow_mut() = Some(value);
        Some(value)
    }

    /// A non-string primitive prototype intrinsic (%Number.prototype%,
    /// %Boolean.prototype%, %BigInt.prototype%, %Symbol.prototype%), cached
    /// after the first resolution (see the struct field).
    pub fn primitive_prototype(&self, name: &'static str) -> Option<Value> {
        let index = primitive_prototype_index(name);
        if let Some(value) = self.primitive_prototypes.borrow()[index] {
            return Some(value);
        }
        let value = self.get(name)?;
        self.primitive_prototypes.borrow_mut()[index] = Some(value);
        Some(value)
    }

    pub fn define(&self, name: &str, value: Value) {
        self.entries
            .borrow_mut()
            .insert(JsString::from_utf8(name), value);
        // Register an agent-dependent builtin's native handler so a warm
        // call dispatches in O(1) (see `builtins::array::handler_for`);
        // functions without a registered handler (prototypes, plain
        // closures, the eval hosts, the fromAsync continuations) keep the
        // intrinsic-identity chain scan.
        if let Some(function) = value.as_function()
            && let Some(handler) = crate::builtins::array::handler_for(name)
                .or_else(|| crate::builtins::regexp::handler_for(name))
                .or_else(|| crate::builtins::string::handler_for(name))
        {
            crate::function::register_builtin_handler(function.id(), handler);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }

    /// Whether the registry holds `value` — the owning-realm lookup for
    /// cross-realm builtin calls (`$262.createRealm` fixtures).
    pub fn contains(&self, value: &Value) -> bool {
        self.entries.borrow().values().any(|entry| entry == value)
    }

    /// Every registered intrinsic value, for post-install linking.
    pub fn entries(&self) -> Vec<Value> {
        self.entries.borrow().values().cloned().collect()
    }
}

fn as_object(value: &Value) -> Option<Handle<JsObject>> {
    value.as_object()
}

/// InitializeHostDefinedRealm (spec 9.3.4): CreateIntrinsics, the global
/// object, NewGlobalEnvironment, and SetDefaultGlobalBindings. The caller
/// pushes the bootstrap execution context.
pub fn initialize_host_defined_realm(agent: &Agent) -> Result<Handle<Realm>, JsError> {
    let intrinsics = Intrinsics::default();
    // The global object's prototype is %Object.prototype% once the intrinsic
    // table is populated (Phase 5+); until then it is null.
    let global = JsObject::ordinary_object_create(
        intrinsics
            .get("%Object.prototype%")
            .and_then(|v| as_object(&v)),
    );
    let global_env = new_global_environment(global, global);
    let realm = Handle::new(Realm {
        agent_signifier: agent.signifier,
        intrinsics,
        global_object: global,
        global_env,
        loaded_modules: RefCell::new(std::collections::HashMap::new()),
    });
    set_default_global_bindings(&realm)?;
    agent.realms.borrow_mut().push(realm);
    agent.realm_count.set(agent.realm_count.get() + 1);
    Ok(realm)
}

/// SetDefaultGlobalBindings (spec 9.3.5). Phase 4 installs the global
/// object's value properties (spec sec-value-properties-of-the-global-object);
/// the function and constructor properties arrive with their built-ins
/// (Phase 8+).
fn set_default_global_bindings(realm: &Handle<Realm>) -> Result<(), JsError> {
    let global = &realm.global_object;
    global.define_property_or_throw(
        &JsString::from_utf8("globalThis"),
        &PropertyDescriptor {
            value: Some(Value::Object(*global)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    for (name, value) in [
        ("Infinity", Value::Number(f64::INFINITY)),
        ("NaN", Value::Number(f64::NAN)),
        ("undefined", Value::Undefined),
    ] {
        global.define_property_or_throw(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(value),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(false),
            },
        )?;
    }
    // %eval% (spec 19.2.1): a global whose identity the call evaluator
    // recognizes to perform direct and indirect eval (sec 13.3.6.1). Its
    // native body is a placeholder; dispatch happens before it runs.
    let eval_func = Function::create_builtin(
        Some(JsString::from_utf8("eval")),
        1,
        Box::new(|_, _| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "eval must be called through the evaluator".into(),
            ))
        }),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define("%eval%", Value::Function(eval_func));
    global.define_property_or_throw(
        &JsString::from_utf8("eval"),
        &PropertyDescriptor {
            value: Some(Value::Function(eval_func)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    crate::builtins::object::install(realm)?;
    // The global object's [[Prototype]] is implementation-defined but the
    // host-standard shape (browsers, Node, the test262 harness) inherits
    // %Object.prototype% so `globalThis.toString` and friends resolve.
    if let Some(object_proto) = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value))
    {
        realm.global_object.set_prototype_of(Some(object_proto))?;
    }
    crate::builtins::function::install(realm)?;
    crate::builtins::array::install(realm)?;
    crate::builtins::typed_array::install(realm)?;
    crate::builtins::boolean::install(realm)?;
    crate::builtins::bigint::install(realm)?;
    crate::builtins::date::install(realm)?;
    crate::builtins::symbol::install(realm)?;
    crate::builtins::error::install(realm)?;
    crate::builtins::global::install(realm)?;
    crate::builtins::math::install(realm)?;
    crate::builtins::number::install(realm)?;
    crate::builtins::string::install(realm)?;
    crate::builtins::regexp::install(realm)?;
    crate::builtins::keyed::install(realm)?;
    crate::builtins::array_buffer::install(realm)?;
    crate::builtins::dataview::install(realm)?;
    crate::builtins::atomics::install(realm)?;
    crate::builtins::json::install(realm)?;
    crate::builtins::promise::install(realm)?;
    crate::builtins::module_source::install(realm)?;
    crate::generator::install(realm)?;
    crate::builtins::async_iterator::install(realm)?;
    crate::async_generator::install(realm)?;
    crate::builtins::async_function::install(realm)?;
    crate::builtins::weakref::install(realm)?;
    crate::builtins::iterator::install(realm)?;
    crate::builtins::disposable::install(realm)?;
    crate::builtins::proxy::install(realm)?;
    crate::builtins::reflect::install(realm)?;
    crate::builtins::temporal::install(realm)?;
    crate::builtins::intl::install(realm)?;
    // ES2022+: every built-in iterator prototype object inherits
    // %Iterator.prototype%, which installs after them. Re-parent them now
    // that the whole table is populated.
    if let Some(iterator_proto) = realm
        .intrinsics
        .get("%Iterator.prototype%")
        .and_then(|value| as_object(&value))
    {
        for name in [
            "%Generator.prototype%",
            "%ArrayIteratorPrototype%",
            "%StringIteratorPrototype%",
            "%MapIteratorPrototype%",
            "%SetIteratorPrototype%",
            "%RegExpStringIteratorPrototype%",
        ] {
            if let Some(proto) = realm
                .intrinsics
                .get(name)
                .and_then(|value| as_object(&value))
            {
                proto.set_prototype_of(Some(iterator_proto))?;
            }
        }
    }
    // spec 10.3.1: every built-in function object's [[Prototype]] is
    // %Function.prototype%. Link all intrinsic-registered functions now that
    // the table is full; %Function.prototype% itself keeps %Object.prototype%
    // (setting its own proto would be a cycle, which set_prototype_of
    // rejects), and installs that set a custom [[Prototype]] (the TypedArray
    // kind constructors inherit %TypedArray%) are left alone.
    let function_proto = match realm.intrinsics.get("%Function.prototype%") {
        Some(value) => match value.kind() {
            ValueKind::Function(function) => function.object.handle(),
            _ => None,
        },
        None => None,
    };
    if let Some(function_proto) = function_proto {
        for value in realm.intrinsics.entries() {
            if let ValueKind::Function(function) = value.kind()
                && let Some(object) = function.object.handle()
                && object.get_prototype_of()?.is_none()
            {
                object.set_prototype_of(Some(function_proto))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    #[test]
    fn intrinsics_store_and_lookup_percent_names() {
        let intrinsics = Intrinsics::default();
        assert!(intrinsics.is_empty());
        assert!(intrinsics.get("%Object.prototype%").is_none());
        intrinsics.define("%Object.prototype%", Value::Undefined);
        assert!(!intrinsics.is_empty());
        assert_eq!(intrinsics.get("%Object.prototype%"), Some(Value::Undefined));
    }

    #[test]
    fn host_defined_realm_installs_value_properties() {
        let agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        let global = &realm.global_object;
        // globalThis points back at the global object...
        assert_eq!(
            global.get(&JsString::from_utf8("globalThis")).unwrap(),
            Value::Object(*global)
        );
        // ...and the non-configurable value properties exist.
        assert_eq!(
            global.get(&JsString::from_utf8("Infinity")).unwrap(),
            Value::Number(f64::INFINITY)
        );
        assert!(matches!(
            global.get(&JsString::from_utf8("NaN")).unwrap().kind(),
            ValueKind::Number(n) if n.is_nan()
        ));
        assert_eq!(
            global.get(&JsString::from_utf8("undefined")).unwrap(),
            Value::Undefined
        );
        let infinity = global
            .get_own_property(&JsString::from_utf8("Infinity"))
            .unwrap()
            .unwrap();
        assert_eq!(infinity.writable(), Some(false));
        assert!(!infinity.enumerable && !infinity.configurable);
        // The global environment is reachable and supplies `this`.
        assert_eq!(
            realm.global_env.get_this_binding().unwrap(),
            Value::Object(*global)
        );
        // Binding an identifier through the global env works end to end.
        realm
            .global_env
            .create_mutable_binding(&JsString::from_utf8("x"), false)
            .unwrap();
        realm
            .global_env
            .initialize_binding(&JsString::from_utf8("x"), Value::Number(42.0))
            .unwrap();
        assert_eq!(
            realm
                .global_env
                .get_binding_value(&JsString::from_utf8("x"), true)
                .unwrap(),
            Value::Number(42.0)
        );
    }

    #[test]
    fn global_property_completeness_check() {
        // The global property list installed so far (spec 19.1-19.3, Phase 8
        // slice). Every entry must be present, writable, non-enumerable, and
        // configurable (except the non-configurable value properties).
        let agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        let global = &realm.global_object;
        let expected = [
            "globalThis",
            "Infinity",
            "NaN",
            "undefined",
            "eval",
            "isFinite",
            "isNaN",
            "parseFloat",
            "parseInt",
            "encodeURI",
            "encodeURIComponent",
            "decodeURI",
            "decodeURIComponent",
            "Object",
            "Function",
            "Boolean",
            "Symbol",
            "Error",
            "EvalError",
            "RangeError",
            "ReferenceError",
            "SyntaxError",
            "TypeError",
            "URIError",
            "AggregateError",
            "SuppressedError",
            "Promise",
            "Map",
            "Set",
            "WeakMap",
            "WeakSet",
        ];
        for name in expected {
            assert!(
                global.has_own_property(&JsString::from_utf8(name)).unwrap(),
                "missing global property {name}"
            );
        }
        // Function properties are non-enumerable and configurable.
        let descriptor = global
            .get_own_property(&JsString::from_utf8("parseInt"))
            .unwrap()
            .unwrap();
        assert!(!descriptor.enumerable && descriptor.configurable);
        assert_eq!(descriptor.writable(), Some(true));
    }
}
