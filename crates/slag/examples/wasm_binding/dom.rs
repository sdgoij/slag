//! A minimal host DOM bridge for the dogfood experiment, kept *outside* the
//! engine: the bridge registers its globals, methods, and element properties
//! through the public `slag` embedding API, and keeps `crux` only for the one
//! host object the facade cannot express (the dynamically-keyed `dataset`).
//!
//! [`install`] gives the running realm a `document` global whose elements
//! are host exotic objects: `textContent`-style property reads/writes and
//! `addEventListener` calls route to the browser through the [`DomHost`]
//! callbacks this module installs. Native events come back into the engine
//! through [`fire`] (exported by the binding as `slag_dom_event`), which
//! calls the registered listener functions with a small event object.
//!
//! The bridge speaks to the host over four wasm imports; host responses
//! (strings, numbers, element ids) are written back into a scratch buffer in
//! linear memory, prefixed with a one-byte type tag.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use crux::handle::Handle;
use crux::host::HostOps;
use crux::object::JsObject as CruxObject;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use slag::{Context, ErrorKind, FunctionCall, HostFn, JsError, JsObject, JsValue};

/// Where the listener registry is rooted on the global (a key no script
/// would use). Listener functions live as own properties of an ordinary
/// object here, so the GC traces them.
const REGISTRY_KEY: &str = "\u{0}slag.dom.registry";

/// Where each element host object stores its node id (an own property the
/// host ops fall through to, so methods like `appendChild` can recover it).
const HIDDEN_ID_KEY: &str = "\u{0}slag.dom.id";

/// Element properties that round-trip to the browser. Reads/writes of
/// anything else fall back to ordinary behaviour (methods are own data
/// properties; unknown names read `undefined`).
const VALUE_PROPS: &[&str] = &[
    "textContent",
    "value",
    "className",
    "innerHTML",
    "title",
    "id",
    "placeholder",
    "hidden",
    "disabled",
    "scrollTop",
    "scrollHeight",
];

/// A value crossing the DOM bridge. Elements travel as opaque ids the host
/// maps back to real nodes.
#[derive(Debug, Clone)]
pub struct HostDomValue {
    tag: u8,
    payload: Vec<u8>,
}

impl HostDomValue {
    const NULL: u8 = 0;
    const BOOL: u8 = 1;
    const NUMBER: u8 = 2;
    const TEXT: u8 = 3;
    const NODE: u8 = 4;

    fn null() -> Self {
        HostDomValue {
            tag: Self::NULL,
            payload: Vec::new(),
        }
    }
    fn boolean(value: bool) -> Self {
        HostDomValue {
            tag: Self::BOOL,
            payload: vec![u8::from(value)],
        }
    }
    fn number(value: f64) -> Self {
        HostDomValue {
            tag: Self::NUMBER,
            payload: value.to_le_bytes().to_vec(),
        }
    }
    fn text(value: impl Into<String>) -> Self {
        HostDomValue {
            tag: Self::TEXT,
            payload: value.into().into_bytes(),
        }
    }

    fn decode(bytes: &[u8]) -> HostDomValue {
        let Some((&tag, payload)) = bytes.split_first() else {
            return HostDomValue::null();
        };
        HostDomValue {
            tag,
            payload: payload.to_vec(),
        }
    }
}

type DomGet = dyn Fn(u32, &str) -> HostDomValue;
type DomSet = dyn Fn(u32, &str, &HostDomValue);
type DomById = dyn Fn(&str) -> Option<u32>;
type DomCreate = dyn Fn(&str) -> Option<u32>;
type DomAttach = dyn Fn(u32, &str);
type DomAppend = dyn Fn(u32, u32);
type DomStorageGet = dyn Fn(&str) -> Option<String>;
type DomStorageSet = dyn Fn(&str, &str);
type DomQuery = dyn Fn(&str) -> Vec<u32>;
type DomClass = dyn Fn(u32, u8, &str, Option<bool>) -> bool;
type DomFocus = dyn Fn(u32);
type DomRoot = dyn Fn() -> Option<u32>;
type DomCopy = dyn Fn(&str) -> bool;
type DomUserRun = dyn Fn(&str);
type DomUserReset = dyn Fn();
type DomDatasetGet = dyn Fn(u32, &str) -> Option<String>;
type DomDatasetSet = dyn Fn(u32, &str, &str);

/// The host side of the bridge; every field is optional so a read-only or
/// no-op surface is easy (`Default` gives the no-op one).
#[derive(Default)]
pub struct DomHost {
    /// Read a property of the node `id`.
    pub get_property: Option<Box<DomGet>>,
    /// Write a property of the node `id`.
    pub set_property: Option<Box<DomSet>>,
    /// `document.getElementById(name)` — the node's id, or `None`.
    pub element_by_id: Option<Box<DomById>>,
    /// `document.createElement(tag)` — the new node's id, or `None`.
    pub create_element: Option<Box<DomCreate>>,
    /// `document.querySelectorAll(selector)` — matching node ids.
    pub query_all: Option<Box<DomQuery>>,
    /// `classList` op (0 add, 1 remove, 2 toggle, 3 contains) — returns
    /// whether the class is present afterwards.
    pub class_op: Option<Box<DomClass>>,
    /// Ask the host to register a native listener for `(id, type)` that
    /// calls back into the engine when the event fires.
    pub attach_listener: Option<Box<DomAttach>>,
    /// `parent.appendChild(child)`.
    pub append_child: Option<Box<DomAppend>>,
    /// `el.focus()`.
    pub focus_node: Option<Box<DomFocus>>,
    /// The `document.documentElement` node id, if the host exposes one.
    pub root_node: Option<Box<DomRoot>>,
    /// Copy `text` to the host clipboard; returns whether it was written.
    pub copy_text: Option<Box<DomCopy>>,
    /// Ask the host to evaluate `source` in the sandbox realm (the host
    /// defers the run until the current engine call has returned).
    pub user_run: Option<Box<DomUserRun>>,
    /// Ask the host to drop the sandbox realm.
    pub user_reset: Option<Box<DomUserReset>>,
    /// `el.dataset[name]` read.
    pub dataset_get: Option<Box<DomDatasetGet>>,
    /// `el.dataset[name] = value` write.
    pub dataset_set: Option<Box<DomDatasetSet>>,
    /// `localStorage.getItem(key)`.
    pub storage_get: Option<Box<DomStorageGet>>,
    /// `localStorage.setItem(key, value)`.
    pub storage_set: Option<Box<DomStorageSet>>,
}

impl DomHost {
    fn get_property(&self, id: u32, name: &str) -> HostDomValue {
        match &self.get_property {
            Some(get) => get(id, name),
            None => HostDomValue::null(),
        }
    }

    fn set_property(&self, id: u32, name: &str, value: &HostDomValue) {
        if let Some(set) = &self.set_property {
            set(id, name, value);
        }
    }

    fn element_by_id(&self, name: &str) -> Option<u32> {
        self.element_by_id.as_ref().and_then(|by_id| by_id(name))
    }

    fn create_element(&self, tag: &str) -> Option<u32> {
        self.create_element.as_ref().and_then(|create| create(tag))
    }

    fn attach_listener(&self, id: u32, event_type: &str) {
        if let Some(attach) = &self.attach_listener {
            attach(id, event_type);
        }
    }

    fn append_child(&self, parent: u32, child: u32) {
        if let Some(append) = &self.append_child {
            append(parent, child);
        }
    }

    fn storage_get(&self, key: &str) -> Option<String> {
        self.storage_get.as_ref().and_then(|get| get(key))
    }

    fn storage_set(&self, key: &str, value: &str) {
        if let Some(set) = &self.storage_set {
            set(key, value);
        }
    }

    fn query_all(&self, selector: &str) -> Vec<u32> {
        self.query_all
            .as_ref()
            .map(|query| query(selector))
            .unwrap_or_default()
    }

    fn class_op(&self, id: u32, op: u8, token: &str, force: Option<bool>) -> bool {
        self.class_op
            .as_ref()
            .map(|op_fn| op_fn(id, op, token, force))
            .unwrap_or(false)
    }

    fn focus_node(&self, id: u32) {
        if let Some(focus) = &self.focus_node {
            focus(id);
        }
    }

    fn root_node(&self) -> Option<u32> {
        self.root_node.as_ref().and_then(|root| root())
    }

    fn copy_text(&self, text: &str) -> bool {
        self.copy_text
            .as_ref()
            .map(|copy| copy(text))
            .unwrap_or(false)
    }

    fn user_run(&self, source: &str) {
        if let Some(run) = &self.user_run {
            run(source);
        }
    }

    fn user_reset(&self) {
        if let Some(reset) = &self.user_reset {
            reset();
        }
    }

    fn dataset_get(&self, id: u32, name: &str) -> Option<String> {
        self.dataset_get.as_ref().and_then(|get| get(id, name))
    }

    fn dataset_set(&self, id: u32, name: &str, value: &str) {
        if let Some(set) = &self.dataset_set {
            set(id, name, value);
        }
    }
}

/// Shared bridge state: the host callbacks, the listeners already attached
/// to the browser, and the registry object that roots listener functions.
struct State {
    host: DomHost,
    attached: RefCell<HashSet<(u32, String)>>,
    registry: JsObject,
}

/// The `el.dataset` object: any property read/write maps to a `data-*`
/// attribute through the host.
struct DatasetOps {
    id: u32,
    state: Rc<State>,
}

impl std::fmt::Debug for DatasetOps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatasetOps").field("id", &self.id).finish()
    }
}

impl HostOps for DatasetOps {
    fn get(
        &self,
        _object: &CruxObject,
        key: &PropertyKey,
        _receiver: &Value,
    ) -> Option<Result<Value, JsError>> {
        let PropertyKey::String(atom) = key else {
            return None;
        };
        let name = crux::string::lookup(*atom).to_string_lossy();
        Some(Ok(match self.state.host.dataset_get(self.id, &name) {
            Some(value) => Value::String(Handle::new(JsString::from_utf8(&value))),
            None => Value::Null,
        }))
    }

    fn set(
        &self,
        _object: &CruxObject,
        key: &PropertyKey,
        value: &Value,
        _receiver: &Value,
    ) -> Option<Result<bool, JsError>> {
        let PropertyKey::String(atom) = key else {
            return None;
        };
        let name = crux::string::lookup(*atom).to_string_lossy();
        let text = match js_to_host_value(value) {
            HostDomValue {
                tag: HostDomValue::TEXT,
                payload,
            } => String::from_utf8_lossy(&payload).into_owned(),
            _ => String::new(),
        };
        self.state.host.dataset_set(self.id, &name, &text);
        Some(Ok(true))
    }
}

/// Map a bridge value to the engine value it represents.
fn host_value_to_js(value: &HostDomValue) -> Value {
    match value.tag {
        HostDomValue::NULL => Value::Null,
        HostDomValue::BOOL => Value::Boolean(value.payload.first() == Some(&1)),
        HostDomValue::NUMBER => {
            let bytes: [u8; 8] = value.payload.as_slice().try_into().unwrap_or([0; 8]);
            Value::Number(f64::from_le_bytes(bytes))
        }
        HostDomValue::TEXT => Value::String(Handle::new(JsString::from_utf8(
            &String::from_utf8_lossy(&value.payload),
        ))),
        HostDomValue::NODE => {
            let bytes: [u8; 4] = value.payload.as_slice().try_into().unwrap_or([0; 4]);
            Value::Number(f64::from(u32::from_le_bytes(bytes)))
        }
        _ => Value::Null,
    }
}

/// Map an engine value to the bridge form. Objects/functions would need an
/// agent to `ToString`; the bridge degrades them (the demo only sets
/// strings/numbers/booleans).
fn js_to_host_value(value: &Value) -> HostDomValue {
    match value.kind() {
        ValueKind::Undefined | ValueKind::Null => HostDomValue::null(),
        ValueKind::Boolean(boolean) => HostDomValue::boolean(boolean),
        ValueKind::Number(number) => HostDomValue::number(number),
        ValueKind::String(text) => HostDomValue::text(text.to_string_lossy()),
        _ => HostDomValue::text("[object Object]"),
    }
}

/// The value-creation surface the bridge needs. During [`install`] a
/// [`Context`] provides it; inside a host callback the re-entrant
/// [`FunctionCall`] does, because the elements a method returns must be built
/// while the call is running.
trait HostApi {
    fn make_object(&self) -> Result<JsObject, JsError>;
    fn make_function(&self, name: &str, length: u32, callback: HostFn) -> Result<JsValue, JsError>;
    fn make_accessor(
        &self,
        target: &JsObject,
        key: &str,
        get: Option<HostFn>,
        set: Option<HostFn>,
    ) -> Result<(), JsError>;
}

impl HostApi for Context {
    fn make_object(&self) -> Result<JsObject, JsError> {
        Context::create_object(self)
    }

    fn make_function(&self, name: &str, length: u32, callback: HostFn) -> Result<JsValue, JsError> {
        Context::create_function(self, name, length, callback)
    }

    fn make_accessor(
        &self,
        target: &JsObject,
        key: &str,
        get: Option<HostFn>,
        set: Option<HostFn>,
    ) -> Result<(), JsError> {
        Context::define_accessor(self, target, key, get, set)
    }
}

impl HostApi for FunctionCall<'_> {
    fn make_object(&self) -> Result<JsObject, JsError> {
        FunctionCall::create_object(self)
    }

    fn make_function(&self, name: &str, length: u32, callback: HostFn) -> Result<JsValue, JsError> {
        FunctionCall::create_function(self, name, length, callback)
    }

    fn make_accessor(
        &self,
        target: &JsObject,
        key: &str,
        get: Option<HostFn>,
        set: Option<HostFn>,
    ) -> Result<(), JsError> {
        FunctionCall::define_accessor(self, target, key, get, set)
    }
}

/// A `document`/element method body: `(state, element-id, call)`.
type DomMethod = fn(&Rc<State>, u32, &FunctionCall<'_>) -> Result<JsValue, JsError>;

/// Install `document` on the realm's global.
pub fn install(context: &mut Context, host: DomHost) -> Result<(), String> {
    let global = context.global().map_err(|error| error.to_string())?;
    let registry = context.create_object().map_err(|error| error.to_string())?;
    global
        .define(REGISTRY_KEY, registry.as_value(), false, false, false)
        .map_err(|error| error.to_string())?;

    let state = Rc::new(State {
        host,
        attached: RefCell::new(HashSet::new()),
        registry,
    });

    let document = context.create_object().map_err(|error| error.to_string())?;
    for (name, builtin) in [
        ("getElementById", dom_get_element_by_id as DomMethod),
        ("createElement", dom_create_element as DomMethod),
        ("querySelectorAll", dom_query_all as DomMethod),
    ] {
        let state = state.clone();
        let function = context
            .create_function(name, 1, Box::new(move |call| builtin(&state, 0, call)))
            .map_err(|error| error.to_string())?;
        document
            .set(name, function)
            .map_err(|error| error.to_string())?;
    }
    if let Some(root_id) = state.host.root_node() {
        let root = make_element(&*context, &state, root_id).map_err(|error| error.to_string())?;
        document
            .set("documentElement", root.as_value())
            .map_err(|error| error.to_string())?;
    }
    global
        .define("document", document.as_value(), true, false, false)
        .map_err(|error| error.to_string())?;

    let storage = context.create_object().map_err(|error| error.to_string())?;
    for (name, builtin) in [
        ("getItem", dom_storage_get as DomMethod),
        ("setItem", dom_storage_set as DomMethod),
    ] {
        let state = state.clone();
        let function = context
            .create_function(name, 2, Box::new(move |call| builtin(&state, 0, call)))
            .map_err(|error| error.to_string())?;
        storage
            .set(name, function)
            .map_err(|error| error.to_string())?;
    }
    global
        .define("localStorage", storage.as_value(), true, false, false)
        .map_err(|error| error.to_string())?;

    // A small host object for engine-side helpers the demo app needs: the
    // CLI's debug dumps and clipboard copy.
    let host_global = context.create_object().map_err(|error| error.to_string())?;
    let dump_function = context
        .create_function(
            "dump",
            2,
            Box::new(|call| {
                let kind = call
                    .arg(0)
                    .and_then(|value| value.as_number())
                    .unwrap_or(0.0) as u32;
                let Some(source) = call.arg(1).and_then(|value| value.as_string()) else {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "__host.dump: expected a source string".into(),
                    ));
                };
                engine_dump(kind, &source)
                    .map(JsValue::string)
                    .map_err(|message| JsError::new(ErrorKind::TypeError, message))
            }),
        )
        .map_err(|error| error.to_string())?;
    host_global
        .set("dump", dump_function)
        .map_err(|error| error.to_string())?;
    let state_for_copy = state.clone();
    let copy_function = context
        .create_function(
            "copy",
            1,
            Box::new(move |call| {
                let text = call
                    .arg(0)
                    .and_then(|value| value.as_string())
                    .unwrap_or_default();
                Ok(JsValue::boolean(state_for_copy.host.copy_text(&text)))
            }),
        )
        .map_err(|error| error.to_string())?;
    host_global
        .set("copy", copy_function)
        .map_err(|error| error.to_string())?;
    // Run/reset the separate user-sandbox realm (dogfood). The request goes
    // to the host because only it can call the sandbox exports — engine code
    // cannot reach the other Context. The host defers the actual run until
    // the current engine call has returned.
    let state_for_user_run = state.clone();
    let user_run_function = context
        .create_function(
            "userRun",
            1,
            Box::new(move |call| {
                let Some(source) = call.arg(0).and_then(|value| value.as_string()) else {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "__host.userRun: expected a source string".into(),
                    ));
                };
                state_for_user_run.host.user_run(&source);
                Ok(JsValue::undefined())
            }),
        )
        .map_err(|error| error.to_string())?;
    host_global
        .set("userRun", user_run_function)
        .map_err(|error| error.to_string())?;
    let state_for_user_reset = state.clone();
    let user_reset_function = context
        .create_function(
            "userReset",
            0,
            Box::new(move |_| {
                state_for_user_reset.host.user_reset();
                Ok(JsValue::undefined())
            }),
        )
        .map_err(|error| error.to_string())?;
    host_global
        .set("userReset", user_reset_function)
        .map_err(|error| error.to_string())?;
    global
        .define("__host", host_global.as_value(), true, false, false)
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// The CLI's `--dump-tokens`/`--dump-ast`/`--print-bytecode` as pure
/// string renderers (kind 0/1/2), exposed to scripts as `__host.dump`.
fn engine_dump(kind: u32, source: &str) -> Result<String, String> {
    match kind {
        0 => slag::dump::tokens(source),
        1 => slag::dump::ast(source),
        2 => slag::dump::bytecode(source),
        _ => Err(format!("__host.dump: unknown mode {kind}")),
    }
}

/// `localStorage.getItem(key)` — `null` when the host has no value.
fn dom_storage_get(
    state: &Rc<State>,
    _id: u32,
    call: &FunctionCall<'_>,
) -> Result<JsValue, JsError> {
    let Some(key) = call.arg(0).and_then(|value| value.as_string()) else {
        return Ok(JsValue::null());
    };
    match state.host.storage_get(&key) {
        Some(value) => Ok(JsValue::string(value)),
        None => Ok(JsValue::null()),
    }
}

/// `localStorage.setItem(key, value)`.
fn dom_storage_set(
    state: &Rc<State>,
    _id: u32,
    call: &FunctionCall<'_>,
) -> Result<JsValue, JsError> {
    let Some(key) = call.arg(0).and_then(|value| value.as_string()) else {
        return Ok(JsValue::undefined());
    };
    let value = call
        .arg(1)
        .and_then(|value| value.as_string())
        .unwrap_or_default();
    state.host.storage_set(&key, &value);
    Ok(JsValue::undefined())
}

/// `document.getElementById(id)`.
fn dom_get_element_by_id(
    state: &Rc<State>,
    _id: u32,
    call: &FunctionCall<'_>,
) -> Result<JsValue, JsError> {
    let name = call
        .arg(0)
        .and_then(|value| value.as_string())
        .unwrap_or_default();
    match state.host.element_by_id(&name) {
        Some(id) => Ok(make_element(call, state, id)?.as_value()),
        None => Ok(JsValue::null()),
    }
}

/// `document.createElement(tag)`.
fn dom_create_element(
    state: &Rc<State>,
    _id: u32,
    call: &FunctionCall<'_>,
) -> Result<JsValue, JsError> {
    let tag = call
        .arg(0)
        .and_then(|value| value.as_string())
        .unwrap_or_default();
    match state.host.create_element(&tag) {
        Some(id) => Ok(make_element(call, state, id)?.as_value()),
        None => Err(JsError::new(
            ErrorKind::TypeError,
            format!("createElement: the host cannot create <{tag}>"),
        )),
    }
}

/// A fresh element object for `id`: it carries its node id as an own property
/// (so the methods can recover it), the DOM methods, a `classList` object, a
/// `dataset` host object, and an accessor for every browser-backed value
/// property.
fn make_element(api: &impl HostApi, state: &Rc<State>, id: u32) -> Result<JsObject, JsError> {
    let element = api.make_object()?;
    element.define(
        HIDDEN_ID_KEY,
        JsValue::number(f64::from(id)),
        false,
        false,
        false,
    )?;

    let dataset = CruxObject::host_object_create(
        Rc::new(DatasetOps {
            id,
            state: state.clone(),
        }),
        None,
    );
    element.set("dataset", JsValue::from(Value::Object(dataset)))?;
    element.set("classList", class_list_object(api, state, id)?.as_value())?;

    for (name, builtin) in [
        ("addEventListener", dom_add_event_listener as DomMethod),
        ("appendChild", dom_append_child as DomMethod),
        ("focus", dom_focus as DomMethod),
    ] {
        let state = state.clone();
        let function =
            api.make_function(name, 2, Box::new(move |call| builtin(&state, id, call)))?;
        element.set(name, function)?;
    }

    for name in VALUE_PROPS {
        let (get_state, get_name) = (state.clone(), (*name).to_string());
        let (set_state, set_name) = (state.clone(), (*name).to_string());
        api.make_accessor(
            &element,
            name,
            Some(Box::new(move |_| {
                Ok(JsValue::from(host_value_to_js(
                    &get_state.host.get_property(id, &get_name),
                )))
            })),
            Some(Box::new(move |call| {
                let value = call.arg(0).unwrap_or_else(JsValue::undefined);
                set_state
                    .host
                    .set_property(id, &set_name, &js_to_host_value(value.value()));
                Ok(JsValue::undefined())
            })),
        )?;
    }
    Ok(element)
}

/// The node id an element wraps, read from its hidden property.
fn element_id_of(value: &JsValue) -> Option<u32> {
    let object = value.as_object()?;
    match object.get(HIDDEN_ID_KEY) {
        Ok(id) => id.as_number().map(|number| number as u32),
        Err(_) => None,
    }
}

/// `parent.appendChild(child)` — returns the appended child, like the DOM.
fn dom_append_child(
    state: &Rc<State>,
    id: u32,
    call: &FunctionCall<'_>,
) -> Result<JsValue, JsError> {
    let Some(child) = call.arg(0) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "appendChild: expected an element".into(),
        ));
    };
    match element_id_of(&child) {
        Some(child_id) => {
            state.host.append_child(id, child_id);
            Ok(child)
        }
        None => Err(JsError::new(
            ErrorKind::TypeError,
            "appendChild: expected an element created by this page".into(),
        )),
    }
}

/// `el.focus()` — give the native node keyboard focus.
fn dom_focus(state: &Rc<State>, id: u32, _call: &FunctionCall<'_>) -> Result<JsValue, JsError> {
    state.host.focus_node(id);
    Ok(JsValue::undefined())
}

/// `document.querySelectorAll(selector)` — an array-like list of element
/// wrappers (`length` + numeric properties), enough for `Array.from`.
fn dom_query_all(state: &Rc<State>, _id: u32, call: &FunctionCall<'_>) -> Result<JsValue, JsError> {
    let selector = call
        .arg(0)
        .and_then(|value| value.as_string())
        .unwrap_or_default();
    let ids = state.host.query_all(&selector);
    let list = call.make_object()?;
    for (index, id) in ids.iter().enumerate() {
        list.set(
            &index.to_string(),
            make_element(call, state, *id)?.as_value(),
        )?;
    }
    list.set("length", JsValue::number(ids.len() as f64))?;
    Ok(list.as_value())
}

/// A `classList` object for one element: `add`/`remove`/`toggle`/`contains`
/// builtins that forward to the host's class op.
fn class_list_object(api: &impl HostApi, state: &Rc<State>, id: u32) -> Result<JsObject, JsError> {
    let list = api.make_object()?;
    for (name, op) in [
        ("add", 0u8),
        ("remove", 1u8),
        ("toggle", 2u8),
        ("contains", 3u8),
    ] {
        let state = state.clone();
        let method = api.make_function(
            name,
            1,
            Box::new(move |call| {
                let token = call
                    .arg(0)
                    .and_then(|value| value.as_string())
                    .unwrap_or_default();
                let force = call.arg(1).and_then(|value| value.as_boolean());
                Ok(JsValue::boolean(state.host.class_op(id, op, &token, force)))
            }),
        )?;
        list.set(name, method)?;
    }
    Ok(list)
}

/// `el.addEventListener(type, listener)`: store the listener on the rooted
/// registry (keyed `id:type`, one bucket object per pair) and ask the host to
/// attach a native listener once.
fn dom_add_event_listener(
    state: &Rc<State>,
    id: u32,
    call: &FunctionCall<'_>,
) -> Result<JsValue, JsError> {
    let Some(event_type) = call.arg(0).and_then(|value| value.as_string()) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "addEventListener: expected an event type string".into(),
        ));
    };
    let listener = call.arg(1).unwrap_or_else(JsValue::undefined);
    if !listener.is_callable() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "addEventListener: listener is not a function".into(),
        ));
    }

    let bucket_key = format!("{id}:{event_type}");
    let bucket = match state.registry.get(&bucket_key)?.as_object() {
        Some(bucket) => bucket,
        None => {
            let bucket = call.make_object()?;
            state.registry.set(&bucket_key, bucket.as_value())?;
            bucket
        }
    };

    let length = bucket.get("length")?.as_number().unwrap_or(0.0) as u64;
    bucket.set(&length.to_string(), listener)?;
    bucket.set("length", JsValue::number((length + 1) as f64))?;

    if state.attached.borrow_mut().insert((id, event_type.clone())) {
        state.host.attach_listener(id, &event_type);
    }
    Ok(JsValue::undefined())
}

/// Fire a native event into the engine: call every listener registered for
/// `(id, type)` with a small `{ type, key, ctrlKey, ... }` event object.
/// `props` is a byte buffer of `(name, value)` entries — one byte of name
/// length, the UTF-8 name, then a tag byte + payload. Returns whether any
/// listener called `preventDefault()`.
pub fn fire(
    context: &mut Context,
    id: u32,
    event_type: &str,
    props: &[u8],
) -> Result<bool, String> {
    let registry_value = context
        .global()
        .and_then(|global| global.get(REGISTRY_KEY))
        .map_err(|error| error.to_string())?;
    let Some(registry) = registry_value.as_object() else {
        return Ok(false);
    };
    let bucket_key = format!("{id}:{event_type}");
    let Some(bucket) = (match registry.get(&bucket_key) {
        Ok(value) => value.as_object(),
        Err(_) => None,
    }) else {
        return Ok(false);
    };

    let length = bucket
        .get("length")
        .ok()
        .and_then(|value| value.as_number())
        .unwrap_or(0.0) as u64;

    let event = context.create_object().map_err(|error| error.to_string())?;
    event
        .set("type", JsValue::string(event_type))
        .map_err(|error| error.to_string())?;
    for (name, value) in decode_event_props(props) {
        event
            .set(&name, JsValue::from(host_value_to_js(&value)))
            .map_err(|error| error.to_string())?;
    }
    // preventDefault/stopPropagation are engine-side stubs; the prevented
    // flag comes back through the export so the host can cancel the native
    // event.
    let prevented = Rc::new(std::cell::Cell::new(false));
    let mark = prevented.clone();
    let prevent_default = context
        .create_function(
            "preventDefault",
            0,
            Box::new(move |_| {
                mark.set(true);
                Ok(JsValue::undefined())
            }),
        )
        .map_err(|error| error.to_string())?;
    event
        .set("preventDefault", prevent_default)
        .map_err(|error| error.to_string())?;
    let event = event.as_value();

    for index in 0..length {
        let listener = bucket
            .get(&index.to_string())
            .map_err(|error| error.to_string())?;
        context
            .call(&listener, &event, std::slice::from_ref(&event))
            .map_err(|error| error.to_string())?;
    }
    Ok(prevented.get())
}

/// Decode the host's event props buffer: repeated `name` + typed value
/// entries (see [`fire`]). Each value carries a u32 (LE) length prefix so
/// string payloads can be delimited inside the concatenated buffer.
fn decode_event_props(bytes: &[u8]) -> Vec<(String, HostDomValue)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 5 <= bytes.len() {
        let name_len = bytes[i] as usize;
        let name_start = i + 1;
        let len_start = name_start + name_len;
        if len_start + 4 > bytes.len() {
            break;
        }
        let value_len = u32::from_le_bytes(
            bytes[len_start..len_start + 4]
                .try_into()
                .expect("4-byte length prefix"),
        ) as usize;
        let value_start = len_start + 4;
        if value_start + value_len > bytes.len() {
            break;
        }
        let name = String::from_utf8_lossy(&bytes[name_start..len_start]).into_owned();
        let value = HostDomValue::decode(&bytes[value_start..value_start + value_len]);
        out.push((name, value));
        i = value_start + value_len;
    }
    out
}

// ---- the wasm host glue (browser DOM behind four env imports) ----

/// Scratch buffer the host writes responses into (typed, length-prefixed by
/// the return value). 64 KiB is plenty for a demo page.
const SCRATCH_LEN: usize = 1 << 16;
static mut SCRATCH: [u8; SCRATCH_LEN] = [0; SCRATCH_LEN];

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn slag_host_dom_get(
        id: u32,
        name_ptr: u32,
        name_len: u32,
        resp_ptr: u32,
        resp_cap: u32,
    ) -> u32;
    fn slag_host_dom_set(id: u32, name_ptr: u32, name_len: u32, value_ptr: u32, value_len: u32);
    fn slag_host_dom_by_id(name_ptr: u32, name_len: u32) -> i32;
    fn slag_host_dom_create(tag_ptr: u32, tag_len: u32) -> i32;
    fn slag_host_dom_append(parent: u32, child: u32);
    fn slag_host_dom_query(sel_ptr: u32, sel_len: u32, resp_ptr: u32, resp_cap: u32) -> u32;
    fn slag_host_dom_class(id: u32, op: i32, force: i32, token_ptr: u32, token_len: u32) -> i32;
    fn slag_host_dom_focus(id: u32);
    fn slag_host_dom_root() -> i32;
    fn slag_host_dom_dataset_get(
        id: u32,
        name_ptr: u32,
        name_len: u32,
        resp_ptr: u32,
        resp_cap: u32,
    ) -> u32;
    fn slag_host_dom_dataset_set(
        id: u32,
        name_ptr: u32,
        name_len: u32,
        value_ptr: u32,
        value_len: u32,
    );
    fn slag_host_copy(text_ptr: u32, text_len: u32) -> i32;
    fn slag_host_user_run(text_ptr: u32, text_len: u32);
    fn slag_host_user_reset();
    fn slag_host_dom_listen(id: u32, type_ptr: u32, type_len: u32);
    fn slag_host_storage_get(key_ptr: u32, key_len: u32, resp_ptr: u32, resp_cap: u32) -> u32;
    fn slag_host_storage_set(key_ptr: u32, key_len: u32, value_ptr: u32, value_len: u32);
}

/// The wasm host callbacks wiring the bridge to the embedding JS.
pub fn dom_host() -> DomHost {
    DomHost {
        get_property: Some(Box::new(|id, name| {
            // SAFETY: single-threaded; the host writes a typed response into
            // SCRATCH (raw pointer, never a static reference) that we read
            // back before returning.
            unsafe {
                let scratch = std::ptr::addr_of_mut!(SCRATCH) as usize as u32;
                let written = slag_host_dom_get(
                    id,
                    name.as_ptr() as u32,
                    name.len() as u32,
                    scratch,
                    SCRATCH_LEN as u32,
                ) as usize;
                let bytes = std::slice::from_raw_parts(scratch as *const u8, written);
                HostDomValue::decode(bytes)
            }
        })),
        set_property: Some(Box::new(|id, name, value| {
            let mut bytes = Vec::with_capacity(value.payload.len() + 1);
            bytes.push(value.tag);
            bytes.extend_from_slice(&value.payload);
            // SAFETY: `bytes` lives for the call; single-threaded.
            unsafe {
                slag_host_dom_set(
                    id,
                    name.as_ptr() as u32,
                    name.len() as u32,
                    bytes.as_ptr() as u32,
                    bytes.len() as u32,
                );
            }
        })),
        element_by_id: Some(Box::new(|name| {
            // SAFETY: `name` lives for the call; single-threaded.
            unsafe {
                let id = slag_host_dom_by_id(name.as_ptr() as u32, name.len() as u32);
                (id > 0).then_some(id as u32)
            }
        })),
        create_element: Some(Box::new(|tag| {
            // SAFETY: `tag` lives for the call; single-threaded.
            unsafe {
                let id = slag_host_dom_create(tag.as_ptr() as u32, tag.len() as u32);
                (id > 0).then_some(id as u32)
            }
        })),
        query_all: Some(Box::new(|selector| {
            // SAFETY: single-threaded; the host writes u32 ids into SCRATCH.
            unsafe {
                let scratch = std::ptr::addr_of_mut!(SCRATCH) as usize as u32;
                let count = slag_host_dom_query(
                    selector.as_ptr() as u32,
                    selector.len() as u32,
                    scratch,
                    SCRATCH_LEN as u32,
                ) as usize;
                let bytes = std::slice::from_raw_parts(scratch as *const u8, count * 4);
                (0..count)
                    .map(|index| {
                        u32::from_le_bytes(bytes[index * 4..index * 4 + 4].try_into().expect("u32"))
                    })
                    .collect()
            }
        })),
        class_op: Some(Box::new(|id, op, token, force| {
            // SAFETY: `token` lives for the call; single-threaded.
            unsafe {
                let force_flag = match force {
                    None => -1,
                    Some(true) => 1,
                    Some(false) => 0,
                };
                slag_host_dom_class(
                    id,
                    i32::from(op),
                    force_flag,
                    token.as_ptr() as u32,
                    token.len() as u32,
                ) != 0
            }
        })),
        focus_node: Some(Box::new(|id| {
            // SAFETY: plain node id; single-threaded.
            unsafe {
                slag_host_dom_focus(id);
            }
        })),
        root_node: Some(Box::new(|| {
            // SAFETY: single-threaded.
            unsafe {
                let id = slag_host_dom_root();
                (id > 0).then_some(id as u32)
            }
        })),
        dataset_get: Some(Box::new(|id, name| {
            // SAFETY: single-threaded; typed response written into SCRATCH.
            unsafe {
                let scratch = std::ptr::addr_of_mut!(SCRATCH) as usize as u32;
                let written = slag_host_dom_dataset_get(
                    id,
                    name.as_ptr() as u32,
                    name.len() as u32,
                    scratch,
                    SCRATCH_LEN as u32,
                ) as usize;
                let bytes = std::slice::from_raw_parts(scratch as *const u8, written);
                match HostDomValue::decode(bytes) {
                    HostDomValue {
                        tag: HostDomValue::TEXT,
                        payload,
                    } => Some(String::from_utf8_lossy(&payload).into_owned()),
                    _ => None,
                }
            }
        })),
        dataset_set: Some(Box::new(|id, name, value| {
            // SAFETY: `name`/`value` live for the call; single-threaded.
            unsafe {
                slag_host_dom_dataset_set(
                    id,
                    name.as_ptr() as u32,
                    name.len() as u32,
                    value.as_ptr() as u32,
                    value.len() as u32,
                );
            }
        })),
        copy_text: Some(Box::new(|text| {
            // SAFETY: `text` lives for the call; single-threaded.
            unsafe { slag_host_copy(text.as_ptr() as u32, text.len() as u32) != 0 }
        })),
        user_run: Some(Box::new(|source| {
            // SAFETY: `source` lives for the call; single-threaded. The host
            // only reads it while the import runs and schedules the deferred
            // sandbox eval.
            unsafe {
                slag_host_user_run(source.as_ptr() as u32, source.len() as u32);
            }
        })),
        user_reset: Some(Box::new(|| {
            // SAFETY: plain signal; single-threaded.
            unsafe {
                slag_host_user_reset();
            }
        })),
        attach_listener: Some(Box::new(|id, event_type| {
            // SAFETY: `event_type` lives for the call; single-threaded.
            unsafe {
                slag_host_dom_listen(id, event_type.as_ptr() as u32, event_type.len() as u32);
            }
        })),
        append_child: Some(Box::new(|parent, child| {
            // SAFETY: plain node ids; single-threaded.
            unsafe {
                slag_host_dom_append(parent, child);
            }
        })),
        storage_get: Some(Box::new(|key| {
            // SAFETY: single-threaded; typed response written into SCRATCH.
            unsafe {
                let scratch = std::ptr::addr_of_mut!(SCRATCH) as usize as u32;
                let written = slag_host_storage_get(
                    key.as_ptr() as u32,
                    key.len() as u32,
                    scratch,
                    SCRATCH_LEN as u32,
                ) as usize;
                let bytes = std::slice::from_raw_parts(scratch as *const u8, written);
                match HostDomValue::decode(bytes) {
                    HostDomValue {
                        tag: HostDomValue::TEXT,
                        payload,
                    } => Some(String::from_utf8_lossy(&payload).into_owned()),
                    _ => None,
                }
            }
        })),
        storage_set: Some(Box::new(|key, value| {
            let mut bytes = Vec::with_capacity(value.len() + 1);
            bytes.push(HostDomValue::TEXT);
            bytes.extend_from_slice(value.as_bytes());
            // SAFETY: `bytes` lives for the call; single-threaded.
            unsafe {
                slag_host_storage_set(
                    key.as_ptr() as u32,
                    key.len() as u32,
                    bytes.as_ptr() as u32,
                    bytes.len() as u32,
                );
            }
        })),
    }
}
