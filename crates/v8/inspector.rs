//! The inspector (`v8::inspector`).
//!
//! Slag has no debugger. V8's inspector is a subsystem *inside* the engine —
//! breakpoints, stepping, the protocol agents, a second thread that can stop a
//! running script — and this bridge has nothing to put behind it. Building one
//! is engine work (the plan calls it "the inspector protocol" for that reason),
//! not bridging work.
//!
//! What the bridge *can* serve is the slice of the protocol that only needs to
//! evaluate JavaScript on the running isolate: the `Runtime` domain the REPL
//! drives (`Runtime.enable`, `Runtime.evaluate`, `Runtime.callFunctionOn`). That
//! much is implemented here, against the isolate and realm `create` and
//! `context_created` were handed. Everything a real debugger needs — breakpoints,
//! stepping, `Runtime.getProperties`, the `Debugger`/`Profiler` domains, pausing
//! — is still refused (see [`NO_INSPECTOR`]).
//!
//! The data types are real, and tested: [`StringView`], [`StringBuffer`],
//! [`Channel`] and the client trait are Rust values that do what their names
//! say, so a host's own `ChannelImpl` and `V8InspectorClientImpl` compile
//! against the same shapes `deno_core` implements.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::rc::{Rc, Weak};

use crate::data::{Array, Boolean, Context, Number, Object, String as JsString, Value};
use crate::handle::{Global, Local};
use crate::scope::{ContextScope, PinScope};
use crate::support::{UniquePtr, UniqueRef};
use crate::{Isolate, UnsafeRawIsolatePtr, null, undefined};

/// The one reason every *protocol* operation this bridge does not serve refuses,
/// so a host reads the same sentence wherever it runs into the wall.
const NO_INSPECTOR: &str = "Slag has no inspector: no breakpoints, no stepping, no debug protocol, and no way to stop a running script";

/// The context id reported to a front end (`Runtime.executionContextCreated`).
///
/// The engine has one realm per isolate, so one id is enough — and it must not
/// be zero, which the REPL treats as "no context".
const CONTEXT_ID: i64 = 1;

/// A string the inspector passes around, whose code units may be one byte or
/// two (`v8::inspector::StringView`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringView<'a> {
    /// One byte per unit (Latin-1).
    U8(&'a [u8]),
    /// Two bytes per unit (UTF-16).
    U16(&'a [u16]),
}

impl StringView<'static> {
    /// An empty view (`v8::inspector::StringView::empty`).
    pub fn empty() -> Self {
        Self::U8(&[])
    }
}

impl<'a> From<&'a [u8]> for StringView<'a> {
    fn from(units: &'a [u8]) -> Self {
        Self::U8(units)
    }
}

impl<'a> From<&'a [u16]> for StringView<'a> {
    fn from(units: &'a [u16]) -> Self {
        Self::U16(units)
    }
}

impl StringView<'_> {
    /// Whether the units are one byte wide.
    pub fn is_8bit(&self) -> bool {
        matches!(self, Self::U8(_))
    }

    /// Whether there are no units.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of code units.
    pub fn len(&self) -> usize {
        match self {
            Self::U8(units) => units.len(),
            Self::U16(units) => units.len(),
        }
    }

    /// The one-byte units, when this is a one-byte view.
    pub fn characters8(&self) -> Option<&[u8]> {
        match self {
            Self::U8(units) => Some(units),
            Self::U16(_) => None,
        }
    }

    /// The two-byte units, when this is a two-byte view.
    pub fn characters16(&self) -> Option<&[u16]> {
        match self {
            Self::U16(units) => Some(units),
            Self::U8(_) => None,
        }
    }
}

/// Rendering a view gives the text it holds: one-byte units are Latin-1, two-byte
/// units are UTF-16 (lossily, as a lone surrogate has no text to render).
impl fmt::Display for StringView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::U8(units) => {
                let text: String = units.iter().map(|&unit| unit as char).collect();
                f.write_str(&text)
            }
            Self::U16(units) => f.write_str(&String::from_utf16_lossy(units)),
        }
    }
}

/// A string the inspector owns (`v8::inspector::StringBuffer`).
#[derive(Debug)]
pub struct StringBuffer(String);

impl StringBuffer {
    /// Copy `source` into a buffer (`v8::inspector::StringBuffer::Create`).
    pub fn create(source: StringView<'_>) -> UniquePtr<Self> {
        UniquePtr::from(UniqueRef::new(Self(source.to_string())))
    }

    /// The text (`v8::inspector::StringBuffer::string`).
    pub fn string(&self) -> StringView<'_> {
        StringView::U8(self.0.as_bytes())
    }
}

/// How a host's session sends messages to a debugger front end
/// (`v8::inspector::ChannelImpl`).
pub trait ChannelImpl {
    /// Answer a protocol call.
    fn send_response(&self, call_id: i32, message: UniquePtr<StringBuffer>);

    /// Send a protocol event.
    fn send_notification(&self, message: UniquePtr<StringBuffer>);

    /// Send everything buffered.
    fn flush_protocol_notifications(&self);
}

/// A host's channel to a debugger front end (`v8::inspector::Channel`).
pub struct Channel(Box<dyn ChannelImpl>);

impl Channel {
    /// A channel over `imp` (`v8::inspector::Channel::new`).
    pub fn new(imp: Box<dyn ChannelImpl>) -> Self {
        Self(imp)
    }

    /// Answer a protocol call (`v8::inspector::Channel::sendResponse`).
    pub fn send_response(&self, call_id: i32, message: UniquePtr<StringBuffer>) {
        self.0.send_response(call_id, message);
    }

    /// Send a protocol event (`v8::inspector::Channel::sendNotification`).
    pub fn send_notification(&self, message: UniquePtr<StringBuffer>) {
        self.0.send_notification(message);
    }

    /// Send everything buffered (`v8::inspector::Channel::flushProtocolNotifications`).
    pub fn flush_protocol_notifications(&self) {
        self.0.flush_protocol_notifications();
    }
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Channel(..)")
    }
}

/// A stack trace as the inspector handles it (`v8::inspector::V8StackTrace`).
///
/// The value is empty: [`V8Inspector::create_stack_trace`] answers one so a host
/// has something to pass to `exception_thrown`, and nothing reads it.
pub struct V8StackTrace(PhantomData<*const ()>);

/// What a host tells the inspector while it debuggers
/// (`v8::inspector::V8InspectorClientImpl`).
#[allow(unused_variables)]
pub trait V8InspectorClientImpl {
    /// A pause has begun and the host's loop should run.
    fn run_message_loop_on_pause(&self, context_group_id: i32) {}

    /// The pause is over.
    fn quit_message_loop_on_pause(&self) {}

    /// The host should run until a debugger attaches.
    fn run_if_waiting_for_debugger(&self, context_group_id: i32) {}

    /// An id for a context group, or 0 for the engine to pick one.
    fn generate_unique_id(&self) -> i64 {
        0
    }

    /// A `console` call, for the debugger's console.
    #[allow(clippy::too_many_arguments)]
    fn console_api_message(
        &self,
        context_group_id: i32,
        level: i32,
        message: &StringView,
        url: &StringView,
        line_number: u32,
        column_number: u32,
        stack_trace: &mut V8StackTrace,
    ) {
    }

    /// The default context of a group, for a protocol call that needs one.
    fn ensure_default_context_in_group(&self, context_group_id: i32) -> Option<Local<'_, Context>> {
        None
    }

    /// The URL a resource name maps to, for a stack frame's display.
    fn resource_name_to_url(&self, resource_name: &StringView) -> Option<UniquePtr<StringBuffer>> {
        None
    }
}

/// A host's inspector client (`v8::inspector::V8InspectorClient`).
pub struct V8InspectorClient(
    /// The host's implementation, held only for its `Drop`.
    #[allow(dead_code)]
    Box<dyn V8InspectorClientImpl>,
);

impl fmt::Debug for V8InspectorClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("V8InspectorClient(..)")
    }
}

impl V8InspectorClient {
    /// A client over `imp` (`v8::inspector::V8InspectorClient::new`).
    pub fn new(imp: Box<dyn V8InspectorClientImpl>) -> Self {
        Self(imp)
    }
}

/// Whether a debugger front end is trusted with the full protocol
/// (`v8::inspector::V8InspectorClientTrustLevel`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum V8InspectorClientTrustLevel {
    /// A front end that gets the reduced protocol.
    Untrusted = 0,
    /// A front end that gets all of it.
    FullyTrusted = 1,
}

/// The `objectId`s a front end holds, so a value it was handed keeps its
/// identity across messages (`Runtime.RemoteObjectId`).
///
/// Every entry is a [`Global`], which pins what it names: a handle a front end
/// still holds is a root, which is `v8`'s own semantics.
#[derive(Default)]
struct HandleTable {
    next: u64,
    values: HashMap<String, Global<Value>>,
}

impl HandleTable {
    fn insert(&mut self, value: Global<Value>) -> String {
        let id = format!("slag:{}", self.next);
        self.next += 1;
        self.values.insert(id.clone(), value);
        id
    }

    fn get(&self, id: &str) -> Option<Global<Value>> {
        self.values.get(id).cloned()
    }
}

/// What the inspector and every session it hands out share: the isolate to
/// evaluate on, the realm it was told about, and the handles live front ends
/// hold. A `Rc` because `deno_core` keeps the inspector for the runtime's life
/// and connects a session per worker off it.
struct InspectorShared {
    /// The isolate the runtime runs on. The runtime owns the isolate and the
    /// inspector together, so a session can hold the raw handle and reconstruct
    /// a scope when it evaluates.
    isolate: UnsafeRawIsolatePtr,
    /// The realm `context_created` named, as a persistent handle.
    context: RefCell<Option<Global<Context>>>,
    /// The `objectId`s handed to a front end.
    handles: RefCell<HandleTable>,
    /// The channels of the sessions a front end connected, so an event the
    /// inspector itself raises — `Runtime.exceptionThrown` — can reach every one
    /// of them. `Weak` because a session owns its channel, not the inspector.
    channels: RefCell<Vec<Weak<Channel>>>,
}

impl InspectorShared {
    /// The isolate as an owned handle for a scope to borrow.
    fn isolate(&self) -> Isolate {
        // SAFETY: the runtime keeps the isolate alive for at least as long as
        // it keeps the inspector, and a session only runs under a live runtime.
        unsafe { Isolate::from_raw_isolate_ptr(self.isolate) }
    }

    /// Send an event to every connected session, dropping the channels whose
    /// session is gone.
    fn broadcast(&self, method: &str, params: serde_json::Value) {
        let body = serde_json::json!({ "method": method, "params": params }).to_string();
        self.channels.borrow_mut().retain(|channel| {
            if let Some(channel) = channel.upgrade() {
                channel.send_notification(StringBuffer::create(StringView::from(body.as_bytes())));
                true
            } else {
                false
            }
        });
    }
}

/// The inspector itself (`v8::inspector::V8Inspector`).
///
/// Creation, the context lifecycle and a connection answer; the `Runtime` half
/// of the protocol is served on the realm the runtime named; the rest refuses.
pub struct V8Inspector {
    #[allow(dead_code)]
    _client: V8InspectorClient,
    shared: Rc<InspectorShared>,
}

impl V8Inspector {
    /// Create an inspector for `isolate` (`v8::inspector::V8Inspector::Create`).
    #[allow(clippy::new_ret_no_self)]
    pub fn create(isolate: &mut crate::Isolate, client: V8InspectorClient) -> V8Inspector {
        V8Inspector {
            _client: client,
            shared: Rc::new(InspectorShared {
                // SAFETY: the isolate is alive here, and the runtime keeps it
                // alive for as long as this inspector.
                isolate: unsafe { isolate.as_raw_isolate_ptr() },
                context: RefCell::new(None),
                handles: RefCell::new(HandleTable::default()),
                channels: RefCell::new(Vec::new()),
            }),
        }
    }

    /// Connect a front end to this inspector
    /// (`v8::inspector::V8Inspector::connect`).
    pub fn connect(
        &self,
        context_group_id: i32,
        channel: Channel,
        state: StringView,
        client_trust_level: V8InspectorClientTrustLevel,
    ) -> V8InspectorSession {
        let _ = (context_group_id, state, client_trust_level);
        let channel = Rc::new(channel);
        self.shared
            .channels
            .borrow_mut()
            .push(Rc::downgrade(&channel));
        V8InspectorSession {
            channel,
            shared: self.shared.clone(),
        }
    }

    /// Remember the realm a front end's `Runtime.evaluate` will run in
    /// (`v8::inspector::V8Inspector::contextCreated`).
    pub fn context_created(
        &self,
        context: Local<Context>,
        context_group_id: i32,
        human_readable_name: StringView,
        aux_data: StringView,
    ) {
        let _ = (context_group_id, human_readable_name, aux_data);
        let isolate = self.shared.isolate();
        *self.shared.context.borrow_mut() = Some(Global::new(&isolate, context));
    }

    /// Forget the realm a front end's calls would run in
    /// (`v8::inspector::V8Inspector::contextDestroyed`).
    pub fn context_destroyed(&self, context: Local<Context>) {
        let _ = context;
        *self.shared.context.borrow_mut() = None;
    }

    /// Wrap a stack trace for the inspector
    /// (`v8::inspector::V8Inspector::createStackTrace`).
    ///
    /// Inert: answers an empty trace, which is the one value a host hands back
    /// to [`exception_thrown`](Self::exception_thrown). Nothing reads it.
    pub fn create_stack_trace(
        &self,
        stack_trace: Option<Local<crate::StackTrace>>,
    ) -> UniquePtr<V8StackTrace> {
        let _ = stack_trace;
        UniquePtr::from(UniqueRef::new(V8StackTrace(PhantomData)))
    }

    /// Report an uncaught exception to every connected front end
    /// (`v8::inspector::V8Inspector::exceptionThrown`).
    ///
    /// Emits `Runtime.exceptionThrown`, which is how the REPL surfaces a throw
    /// that happens outside an `evaluate` — an unhandled rejection, a timer.
    #[allow(clippy::too_many_arguments)]
    pub fn exception_thrown(
        &self,
        context: Local<Context>,
        message: StringView,
        exception: Local<Value>,
        detailed_message: StringView,
        url: StringView,
        line_number: u32,
        column_number: u32,
        stack_trace: UniquePtr<V8StackTrace>,
        script_id: i32,
    ) -> u32 {
        let _ = (
            context,
            detailed_message,
            url,
            line_number,
            column_number,
            stack_trace,
            script_id,
        );
        self.shared.broadcast(
            "Runtime.exceptionThrown",
            serde_json::json!({
                "exceptionDetails": {
                    "text": message.to_string(),
                    "exception": {
                        "type": "object",
                        "subtype": "error",
                        "description": self.describe_exception(exception),
                    },
                }
            }),
        );
        1
    }

    /// `String(exception)` in the realm, for the notification's description.
    fn describe_exception(&self, exception: Local<Value>) -> String {
        let mut isolate = self.shared.isolate();
        crate::scope!(let scope, &mut isolate);
        let Some(context) = self
            .shared
            .context
            .borrow()
            .as_ref()
            .map(|global| global.get(scope))
        else {
            return "Uncaught".to_string();
        };
        let scope = &mut ContextScope::new(scope, context);
        exception.to_rust_string_lossy(scope)
    }
}

/// A front end's connection to the inspector
/// (`v8::inspector::V8InspectorSession`).
///
/// Serves the `Runtime` domain the REPL drives; every other protocol method
/// aborts with the reason at the first call.
pub struct V8InspectorSession {
    channel: Rc<Channel>,
    shared: Rc<InspectorShared>,
}

impl V8InspectorSession {
    /// Whether the protocol has a handler for `method`
    /// (`v8::inspector::V8InspectorSession::canDispatchMethod`).
    pub fn can_dispatch_method(method: StringView) -> bool {
        matches!(
            method.to_string().as_str(),
            "Runtime.enable" | "Runtime.evaluate" | "Runtime.callFunctionOn"
        )
    }

    /// Dispatch a protocol message
    /// (`v8::inspector::V8InspectorSession::dispatchProtocolMessage`).
    ///
    /// # Panics
    ///
    /// For every method the `Runtime` slice does not serve: a front end that
    /// asks for the debugger is told at once rather than left waiting.
    pub fn dispatch_protocol_message(&self, message: StringView) {
        let text = message.to_string();
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        let id = request.get("id").and_then(|id| id.as_i64());
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = request
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        match method {
            "Runtime.enable" => {
                if let Some(id) = id {
                    self.respond(id, serde_json::json!({}));
                }
                self.notify(
                    "Runtime.executionContextCreated",
                    serde_json::json!({
                        "context": {
                            "id": CONTEXT_ID,
                            "auxData": { "isDefault": true, "type": "default" },
                        }
                    }),
                );
            }
            "Runtime.evaluate" => {
                let expression = params
                    .get("expression")
                    .and_then(|e| e.as_str())
                    .unwrap_or("");
                if let Some(id) = id {
                    self.respond(id, self.evaluate(expression));
                }
            }
            "Runtime.callFunctionOn" => {
                let declaration = params
                    .get("functionDeclaration")
                    .and_then(|d| d.as_str())
                    .unwrap_or("function () {}");
                let object_id = params.get("objectId").and_then(|o| o.as_str());
                let arguments = params
                    .get("arguments")
                    .and_then(|a| a.as_array())
                    .cloned()
                    .unwrap_or_default();
                if let Some(id) = id {
                    self.respond(
                        id,
                        self.call_function_on(declaration, object_id, &arguments),
                    );
                }
            }
            _ => panic!(
                "v8::inspector::V8InspectorSession::dispatch_protocol_message: {NO_INSPECTOR}"
            ),
        }
    }

    /// Ask to pause at the next statement
    /// (`v8::inspector::V8InspectorSession::schedulePauseOnNextStatement`).
    ///
    /// # Panics
    ///
    /// Always: there is no pause.
    pub fn schedule_pause_on_next_statement(&self, reason: StringView, detail: StringView) {
        let _ = (reason, detail);
        panic!(
            "v8::inspector::V8InspectorSession::schedule_pause_on_next_statement: {NO_INSPECTOR}"
        )
    }

    /// Withdraw a pause request
    /// (`v8::inspector::V8InspectorSession::cancelPauseOnNextStatement`).
    ///
    /// # Panics
    ///
    /// Always: there is no pause.
    pub fn cancel_pause_on_next_statement(&self) {
        panic!("v8::inspector::V8InspectorSession::cancel_pause_on_next_statement: {NO_INSPECTOR}")
    }

    /// Answer a request: `{ "id": id, "result": result }`, the envelope
    /// `deno_core`'s channel forwards verbatim and the REPL unwraps.
    fn respond(&self, id: i64, result: serde_json::Value) {
        let body = serde_json::json!({ "id": id, "result": result }).to_string();
        self.channel.send_response(
            id as i32,
            StringBuffer::create(StringView::from(body.as_bytes())),
        );
    }

    /// Send an event: `{ "method": method, "params": params }`.
    fn notify(&self, method: &str, params: serde_json::Value) {
        let body = serde_json::json!({ "method": method, "params": params }).to_string();
        self.channel
            .send_notification(StringBuffer::create(StringView::from(body.as_bytes())));
    }

    fn context_local<'s>(&self, scope: &PinScope<'s, '_, ()>) -> Option<Local<'s, Context>> {
        self.shared
            .context
            .borrow()
            .as_ref()
            .map(|global| global.get(scope))
    }

    fn lookup_handle<'s, 'i>(
        &self,
        scope: &PinScope<'s, 'i, Context>,
        id: &str,
    ) -> Option<Local<'s, Value>> {
        let global = self.shared.handles.borrow().get(id)?;
        Some(global.get(scope))
    }

    fn store_handle(&self, value: Local<Value>) -> String {
        let isolate = self.shared.isolate();
        let global = Global::new(&isolate, value);
        self.shared.handles.borrow_mut().insert(global)
    }

    /// Evaluate `expression` as a script in the realm and shape the answer as a
    /// `Runtime.evaluate` result: `{ result, exceptionDetails }`.
    fn evaluate(&self, expression: &str) -> serde_json::Value {
        let mut isolate = self.shared.isolate();
        crate::scope!(let scope, &mut isolate);
        let Some(context) = self.context_local(scope) else {
            return missing_context();
        };
        let scope = &mut ContextScope::new(scope, context);
        let Some(code) = JsString::new(scope, expression) else {
            return self.exception_response(scope);
        };
        match crate::Script::compile(scope, code, None) {
            Some(script) => match script.run(scope) {
                Some(value) => serde_json::json!({
                    "result": self.remote_object(scope, value),
                    "exceptionDetails": serde_json::Value::Null,
                }),
                None => self.exception_response(scope),
            },
            None => self.exception_response(scope),
        }
    }

    /// Call a function expression with a receiver and arguments, shaped as a
    /// `Runtime.callFunctionOn` result.
    fn call_function_on(
        &self,
        declaration: &str,
        object_id: Option<&str>,
        arguments: &[serde_json::Value],
    ) -> serde_json::Value {
        let mut isolate = self.shared.isolate();
        crate::scope!(let scope, &mut isolate);
        let Some(context) = self.context_local(scope) else {
            return missing_context();
        };
        let scope = &mut ContextScope::new(scope, context);
        let receiver = match object_id.and_then(|id| self.lookup_handle(scope, id)) {
            Some(value) => value,
            None => {
                // No receiver: the realm's global object, as V8 does when only a
                // context is named.
                let global = Local::<Object>::from_engine(crate::realm_of(scope).global());
                global.into()
            }
        };
        let source = format!("({declaration})");
        let Some(code) = JsString::new(scope, &source) else {
            return self.exception_response(scope);
        };
        let function = match crate::Script::compile(scope, code, None)
            .and_then(|script| script.run(scope))
            .and_then(|value| Local::<crate::Function>::try_from(value).ok())
        {
            Some(function) => function,
            None => return self.exception_response(scope),
        };
        let values: Vec<Local<Value>> = arguments
            .iter()
            .map(|argument| self.call_argument(scope, argument))
            .collect();
        match function.call(scope, receiver, &values) {
            Some(value) => serde_json::json!({
                "result": self.remote_object(scope, value),
                "exceptionDetails": serde_json::Value::Null,
            }),
            None => self.exception_response(scope),
        }
    }

    /// A `CallArgument` as a value: a handle the front end holds, an
    /// unserializable primitive, or a JSON value.
    fn call_argument<'s, 'i>(
        &self,
        scope: &PinScope<'s, 'i, Context>,
        argument: &serde_json::Value,
    ) -> Local<'s, Value> {
        if let Some(id) = argument.get("objectId").and_then(|id| id.as_str())
            && let Some(value) = self.lookup_handle(scope, id)
        {
            return value;
        }
        if let Some(raw) = argument
            .get("unserializableValue")
            .and_then(|raw| raw.as_str())
        {
            match raw {
                "NaN" => return Number::new(scope, f64::NAN).into(),
                "Infinity" => return Number::new(scope, f64::INFINITY).into(),
                "-Infinity" => return Number::new(scope, f64::NEG_INFINITY).into(),
                "-0" => return Number::new(scope, -0.0).into(),
                _ => {
                    if let Some(digits) = raw.strip_suffix('n')
                        && let Ok(value) = digits.parse::<i64>()
                    {
                        return Number::new(scope, value as f64).into();
                    }
                }
            }
        }
        match argument.get("value") {
            Some(serde_json::Value::Null) => null(scope).into(),
            Some(serde_json::Value::Bool(value)) => Boolean::new(scope, *value).into(),
            Some(serde_json::Value::Number(value)) => {
                Number::new(scope, value.as_f64().unwrap_or(f64::NAN)).into()
            }
            Some(serde_json::Value::String(value)) => JsString::new(scope, value)
                .map(|string| string.into())
                .unwrap_or_else(|| undefined(scope).into()),
            _ => undefined(scope).into(),
        }
    }

    /// A value as a `RemoteObject`: primitives by value, objects by handle.
    fn remote_object<'s, 'i>(
        &self,
        scope: &PinScope<'s, 'i, Context>,
        value: Local<Value>,
    ) -> serde_json::Value {
        if value.is_undefined() {
            return serde_json::json!({ "type": "undefined" });
        }
        if value.is_null() {
            return serde_json::json!({
                "type": "object",
                "subtype": "null",
                "value": serde_json::Value::Null,
            });
        }
        if value.is_boolean() {
            return serde_json::json!({ "type": "boolean", "value": value.boolean_value(scope) });
        }
        if value.is_number() {
            return number_object(value.number_value(scope).unwrap_or(f64::NAN));
        }
        if value.is_string() {
            return serde_json::json!({
                "type": "string",
                "value": value.to_rust_string_lossy(scope),
            });
        }
        if value.is_big_int() {
            let text = format!("{}n", value.to_rust_string_lossy(scope));
            return serde_json::json!({
                "type": "bigint",
                "unserializableValue": text.clone(),
                "description": text,
            });
        }
        let object_id = self.store_handle(value);
        if value.is_function() {
            return serde_json::json!({
                "type": "function",
                "className": "Function",
                "description": value.to_rust_string_lossy(scope),
                "objectId": object_id,
            });
        }
        if value.is_array() {
            let length = Local::<Array>::try_from(value)
                .map(|array| array.length())
                .unwrap_or(0);
            return serde_json::json!({
                "type": "object",
                "subtype": "array",
                "className": "Array",
                "description": format!("Array({length})"),
                "objectId": object_id,
            });
        }
        let class_name = Local::<Object>::try_from(value)
            .map(|object| object.get_constructor_name().to_rust_string_lossy(scope))
            .unwrap_or_else(|_| "Object".to_string());
        serde_json::json!({
            "type": "object",
            "className": class_name,
            "description": class_name,
            "objectId": object_id,
        })
    }

    /// The answer to a call whose script threw: the pending exception is taken
    /// off the isolate (so the runtime's own error handling is not confused by
    /// one it did not raise) and reported as `exceptionDetails`.
    fn exception_response<'s, 'i>(&self, scope: &PinScope<'s, 'i, Context>) -> serde_json::Value {
        let description = match scope.engine().take_pending_exception() {
            Some(exception) => {
                Local::<Value>::from_engine(exception.into()).to_rust_string_lossy(scope)
            }
            None => "Uncaught".to_string(),
        };
        let exception = serde_json::json!({
            "type": "object",
            "subtype": "error",
            "description": description,
        });
        serde_json::json!({
            "result": exception.clone(),
            "exceptionDetails": { "text": "Uncaught", "exception": exception },
        })
    }
}

/// The answer when no realm was ever named: the runtime did not call
/// `context_created` before a front end spoke, so there is nothing to evaluate
/// against.
fn missing_context() -> serde_json::Value {
    serde_json::json!({
        "result": { "type": "undefined" },
        "exceptionDetails": serde_json::Value::Null,
    })
}

/// A number as a `RemoteObject`: JSON for the finite ones, an
/// `unserializableValue` for the rest, as V8 reports them.
fn number_object(number: f64) -> serde_json::Value {
    if number.is_nan() {
        return serde_json::json!({
            "type": "number",
            "unserializableValue": "NaN",
            "description": "NaN",
        });
    }
    if number == f64::INFINITY {
        return serde_json::json!({
            "type": "number",
            "unserializableValue": "Infinity",
            "description": "Infinity",
        });
    }
    if number == f64::NEG_INFINITY {
        return serde_json::json!({
            "type": "number",
            "unserializableValue": "-Infinity",
            "description": "-Infinity",
        });
    }
    if number == 0.0 && number.is_sign_negative() {
        return serde_json::json!({
            "type": "number",
            "unserializableValue": "-0",
            "description": "-0",
        });
    }
    let description = format!("{number}");
    serde_json::json!({ "type": "number", "value": number, "description": description })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    /// The data types are real: a view renders the text it holds, whatever the
    /// width of its units.
    #[test]
    fn a_string_view_renders_its_text() {
        let latin1 = StringView::from(&b"caf\xE9"[..]);
        assert!(latin1.is_8bit());
        assert_eq!(latin1.len(), 4);
        assert_eq!(latin1.to_string(), "café");
        assert_eq!(latin1.characters8(), Some(&b"caf\xE9"[..]));
        assert_eq!(latin1.characters16(), None);

        let utf16 = StringView::from(&[0x0063u16, 0x0061, 0x0066, 0x00E9][..]);
        assert!(!utf16.is_8bit());
        assert_eq!(utf16.to_string(), "café");
        assert_eq!(utf16.characters16().map(<[u16]>::len), Some(4));
        assert_eq!(utf16.characters8(), None);

        assert!(StringView::empty().is_empty());
        assert_eq!(StringView::empty().to_string(), "");
    }

    /// A buffer owns its text, and hands it back as a view a host can read.
    #[test]
    fn a_string_buffer_round_trips_through_a_view() {
        let buffer = StringBuffer::create(StringView::from(&b"hello"[..]));
        let mut buffer = buffer;
        assert_eq!(
            buffer.as_mut().expect("not null").string().to_string(),
            "hello"
        );
        assert_eq!(buffer.unwrap().string().to_string(), "hello");
    }

    /// A channel forwards to the host's implementation, which is all a channel
    /// is: the reason a host has one is to receive what a session sends.
    #[test]
    fn a_channel_forwards_to_the_hosts_implementation() {
        #[derive(Default)]
        struct Recording {
            calls: RefCell<Vec<String>>,
        }

        impl ChannelImpl for Recording {
            fn send_response(&self, call_id: i32, message: UniquePtr<StringBuffer>) {
                let text = message.unwrap().string().to_string();
                self.calls
                    .borrow_mut()
                    .push(format!("response {call_id}: {text}"));
            }

            fn send_notification(&self, message: UniquePtr<StringBuffer>) {
                let text = message.unwrap().string().to_string();
                self.calls
                    .borrow_mut()
                    .push(format!("notification: {text}"));
            }

            fn flush_protocol_notifications(&self) {
                self.calls.borrow_mut().push("flush".to_string());
            }
        }

        let recording = Rc::new(Recording::default());
        struct Shared(Rc<Recording>);

        impl ChannelImpl for Shared {
            fn send_response(&self, call_id: i32, message: UniquePtr<StringBuffer>) {
                self.0.send_response(call_id, message);
            }

            fn send_notification(&self, message: UniquePtr<StringBuffer>) {
                self.0.send_notification(message);
            }

            fn flush_protocol_notifications(&self) {
                self.0.flush_protocol_notifications();
            }
        }

        let channel = Channel::new(Box::new(Shared(Rc::clone(&recording))));
        channel.send_response(7, StringBuffer::create(StringView::from(&b"ok"[..])));
        channel.send_notification(StringBuffer::create(StringView::from(&b"event"[..])));
        channel.flush_protocol_notifications();

        assert_eq!(
            recording.calls.borrow().as_slice(),
            ["response 7: ok", "notification: event", "flush"]
        );
    }

    /// A host can *create* an inspector, which is what a runtime that sets
    /// `inspector: true` needs before anything else can run.
    #[test]
    fn an_inspector_is_created() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, isolate);
        let client = V8InspectorClient::new(Box::new(Noop));
        let inspector = V8Inspector::create(scope, client);
        let _ = inspector;
    }

    /// The context lifecycle records the realm a front end's calls will run in.
    #[test]
    fn telling_the_inspector_about_a_context_is_recorded() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, isolate);
        let context = crate::Context::new(scope, Default::default());
        let client = V8InspectorClient::new(Box::new(Noop));
        let inspector = V8Inspector::create(scope, client);

        inspector.context_created(
            context,
            1,
            StringView::from(&b"main realm"[..]),
            StringView::from(&br#"{"isDefault": true, "type": "default"}"#[..]),
        );
        assert!(inspector.shared.context.borrow().is_some());
        inspector.context_destroyed(context);
        assert!(inspector.shared.context.borrow().is_none());
    }

    /// An uncaught exception the runtime reports is broadcast to every connected
    /// front end as `Runtime.exceptionThrown`.
    #[test]
    fn the_inspectors_exception_path_reports() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, isolate);
        let context = crate::Context::new(scope, Default::default());
        let client = V8InspectorClient::new(Box::new(Noop));
        let inspector = V8Inspector::create(scope, client);
        inspector.context_created(context, 1, StringView::empty(), StringView::empty());

        let recording = Rc::new(Recording::default());
        let _session = inspector.connect(
            1,
            Channel::new(Box::new(RecordingChannel(Rc::clone(&recording)))),
            StringView::empty(),
            V8InspectorClientTrustLevel::FullyTrusted,
        );

        let trace = inspector.create_stack_trace(None);
        assert!(!trace.is_null(), "an empty trace, not a null one");
        let thrown: Local<'_, Value> = crate::undefined(scope).into();
        assert_ne!(
            inspector.exception_thrown(
                context,
                StringView::from(&b"Uncaught"[..]),
                thrown,
                StringView::empty(),
                StringView::empty(),
                0,
                0,
                trace,
                0,
            ),
            0,
            "a reported exception is not id zero"
        );
        let calls = recording.calls.borrow();
        let event = calls
            .iter()
            .find(|call| call.contains("Runtime.exceptionThrown"))
            .expect("the exception event");
        assert!(event.contains("\"text\":\"Uncaught\""), "{event}");
        assert!(event.contains("exceptionDetails"), "{event}");
    }

    /// The tier's refusing half, at its new point: a connection succeeds, and
    /// the protocol's unimplemented methods still refuse.
    #[test]
    fn a_session_can_be_connected_and_the_debugger_domain_refuses() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, isolate);
        let client = V8InspectorClient::new(Box::new(Noop));
        let inspector = V8Inspector::create(scope, client);
        let _session = inspector.connect(
            1,
            Channel::new(Box::new(NoopChannel)),
            StringView::empty(),
            V8InspectorClientTrustLevel::FullyTrusted,
        );
        // The Runtime slice the REPL drives is answered, so a host that asks
        // before it sends knows it can send.
        assert!(V8InspectorSession::can_dispatch_method(StringView::from(
            "Runtime.enable".as_bytes()
        )));
        // A debugger's own messages still are not.
        assert!(!V8InspectorSession::can_dispatch_method(StringView::from(
            "Debugger.enable".as_bytes()
        )));
    }

    /// The Runtime domain's `enable`: an empty result and the one
    /// `executionContextCreated` the REPL reads the context id from.
    #[test]
    fn runtime_enable_answers_and_names_the_context() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, isolate);
        let context = crate::Context::new(scope, Default::default());
        let client = V8InspectorClient::new(Box::new(Noop));
        let inspector = V8Inspector::create(scope, client);
        inspector.context_created(context, 1, StringView::empty(), StringView::empty());
        let recording = Rc::new(Recording::default());
        let session = inspector.connect(
            1,
            Channel::new(Box::new(RecordingChannel(Rc::clone(&recording)))),
            StringView::empty(),
            V8InspectorClientTrustLevel::FullyTrusted,
        );

        session.dispatch_protocol_message(StringView::from(
            br#"{"id":7,"method":"Runtime.enable","params":null}"#.as_slice(),
        ));

        let calls = recording.calls.borrow();
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("response 7:") && call.contains("\"result\"")),
            "the request is answered: {calls:?}"
        );
        let created = calls
            .iter()
            .find(|call| call.contains("Runtime.executionContextCreated"))
            .expect("the context event");
        assert!(created.contains("\"isDefault\":true"), "{created}");
        assert!(created.contains("\"id\":1"), "{created}");
    }

    /// The same connection, at a message the debugger domain owns.
    #[test]
    #[should_panic(expected = "Slag has no inspector")]
    fn an_unimplemented_method_refuses_at_the_first_message() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, isolate);
        let client = V8InspectorClient::new(Box::new(Noop));
        let inspector = V8Inspector::create(scope, client);
        let session = inspector.connect(
            1,
            Channel::new(Box::new(NoopChannel)),
            StringView::empty(),
            V8InspectorClientTrustLevel::FullyTrusted,
        );
        session.dispatch_protocol_message(StringView::from(
            br#"{"id":1,"method":"Debugger.enable"}"#.as_slice(),
        ));
    }

    /// The same wall for the pause a debugger schedules, which a host reaches
    /// through a session it already holds.
    #[test]
    #[should_panic(expected = "Slag has no inspector")]
    fn scheduling_a_pause_refuses_with_the_reason() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, isolate);
        let client = V8InspectorClient::new(Box::new(Noop));
        let inspector = V8Inspector::create(scope, client);
        let session = inspector.connect(
            1,
            Channel::new(Box::new(NoopChannel)),
            StringView::empty(),
            V8InspectorClientTrustLevel::FullyTrusted,
        );
        session.schedule_pause_on_next_statement(StringView::empty(), StringView::empty());
    }

    struct Noop;

    impl V8InspectorClientImpl for Noop {}

    struct NoopChannel;

    impl ChannelImpl for NoopChannel {
        fn send_response(&self, _call_id: i32, _message: UniquePtr<StringBuffer>) {}

        fn send_notification(&self, _message: UniquePtr<StringBuffer>) {}

        fn flush_protocol_notifications(&self) {}
    }

    #[derive(Default)]
    struct Recording {
        calls: RefCell<Vec<String>>,
    }

    impl ChannelImpl for Recording {
        fn send_response(&self, call_id: i32, message: UniquePtr<StringBuffer>) {
            let text = message.unwrap().string().to_string();
            self.calls
                .borrow_mut()
                .push(format!("response {call_id}: {text}"));
        }

        fn send_notification(&self, message: UniquePtr<StringBuffer>) {
            let text = message.unwrap().string().to_string();
            self.calls
                .borrow_mut()
                .push(format!("notification: {text}"));
        }

        fn flush_protocol_notifications(&self) {}
    }

    struct RecordingChannel(Rc<Recording>);

    impl ChannelImpl for RecordingChannel {
        fn send_response(&self, call_id: i32, message: UniquePtr<StringBuffer>) {
            self.0.send_response(call_id, message);
        }

        fn send_notification(&self, message: UniquePtr<StringBuffer>) {
            self.0.send_notification(message);
        }

        fn flush_protocol_notifications(&self) {
            self.0.flush_protocol_notifications();
        }
    }

    /// A client is a real wrapper over the host's implementation, so a host's
    /// own trait impl compiles and its defaults are reachable.
    #[test]
    fn a_client_holds_the_hosts_implementation() {
        let client = V8InspectorClient::new(Box::new(Noop));
        let _ = format!("{client:?}");
        let noop = Noop;
        assert_eq!(noop.generate_unique_id(), 0);
        assert!(!V8InspectorSession::can_dispatch_method(StringView::empty()));
    }
}
