//! The inspector (`v8::inspector`).
//!
//! Slag has no debugger. V8's inspector is a subsystem *inside* the engine —
//! breakpoints, stepping, the protocol agents, a second thread that can stop a
//! running script — and this bridge has nothing to put behind it. Building one
//! is engine work (the plan calls it "the inspector protocol" for that reason),
//! not bridging work.
//!
//! The tier splits, and each half says so where a host meets it:
//!
//! * **Creation and the context lifecycle are inert.** [`V8Inspector::create`]
//!   answers a real inspector, and [`V8Inspector::context_created`],
//!   [`context_destroyed`](V8Inspector::context_destroyed) and
//!   [`exception_thrown`](V8Inspector::exception_thrown) record nothing and do
//!   nothing. That is what a host needs to *boot*: `deno_core` sets
//!   `inspector: true` for its main worker and creates one for every runtime,
//!   whether or not a debugger ever connects. None of these calls is read back
//!   by the engine, so a program that never inspects cannot observe them.
//! * **The protocol refuses.** [`V8Inspector::connect`] and everything a session
//!   would answer abort with the reason, so a host that attaches a debugger is
//!   told at once instead of waiting on a session that never answers. Failing
//!   loudly beats two alternatives: a silent session that a debugger hangs on,
//!   and a protocol that answers every method with an error a host cannot
//!   distinguish from a bug.
//!
//! The *data* types here are real, and tested: [`StringView`], [`StringBuffer`],
//! [`Channel`] and the client trait are Rust values that do what their names
//! say, so a host's own `ChannelImpl` and `V8InspectorClientImpl` compile
//! against the same shapes `deno_core` implements.

use std::fmt;
use std::marker::PhantomData;

use crate::data::{Context, Value};
use crate::handle::Local;
use crate::support::{UniquePtr, UniqueRef};

/// The one reason every *protocol* operation here refuses, so a host reads the
/// same sentence wherever it runs into the wall. Creation and the context
/// lifecycle do not use it: they are inert, as the module documentation says.
const NO_INSPECTOR: &str = "Slag has no inspector: no breakpoints, no stepping, no debug protocol, and no way to stop a running script";

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
///
/// The crate we stand in for prints a two-byte view as an array of numbers,
/// which is a rendering of the units rather than of the text; this is the text.
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
///
/// A host implements this; nothing here calls it, because nothing here has
/// messages to send.
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
///
/// Every method has the crate we stand in for's default, so a host implements
/// only what it needs. Nothing here calls them: a client is only reached through
/// a created inspector.
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
    /// The host's implementation, held only for its `Drop`: the engine is what
    /// would call it, and there is no inspector to call it from.
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

/// The inspector itself (`v8::inspector::V8Inspector`).
///
/// An inspector with no debugger behind it.
///
/// **Inert.** [`create`](Self::create) answers one so a host can boot, the
/// context lifecycle records nothing, and the protocol — [`connect`](Self::connect)
/// and the session it would make — refuses. The type exists so a host's own
/// inspector wrapper — `deno_core`'s `JsRuntimeInspector`, which holds an
/// `Rc<V8Inspector>` — has something to name and to hand around.
pub struct V8Inspector {
    /// The client, held as the crate we stand in for holds it: for the
    /// inspector's lifetime. Nothing here calls it, because only a debugger
    /// would.
    _client: V8InspectorClient,
}

impl V8Inspector {
    /// Create an inspector for `isolate` (`v8::inspector::V8Inspector::Create`).
    ///
    /// The inspector is inert: it observes no isolate and reports nothing. What
    /// it buys is that a host which starts a runtime *with* an inspector — which
    /// `deno_core` does for every runtime — runs, and what it does not buy is a
    /// debugger: a host that attaches one still meets the wall at
    /// [`connect`](Self::connect).
    #[allow(clippy::new_ret_no_self)]
    pub fn create(isolate: &mut crate::Isolate, client: V8InspectorClient) -> V8Inspector {
        let _ = isolate;
        V8Inspector { _client: client }
    }

    /// Connect a front end to this inspector
    /// (`v8::inspector::V8Inspector::connect`).
    ///
    /// # Panics
    ///
    /// Always: the protocol is the half of this module that refuses. A host
    /// that attaches a debugger is told at once rather than handed a session
    /// that would never answer it.
    pub fn connect(
        &self,
        context_group_id: i32,
        channel: Channel,
        state: StringView,
        client_trust_level: V8InspectorClientTrustLevel,
    ) -> V8InspectorSession {
        let _ = (context_group_id, channel, state, client_trust_level);
        panic!("v8::inspector::V8Inspector::connect: {NO_INSPECTOR}")
    }

    /// Tell the inspector about a context
    /// (`v8::inspector::V8Inspector::contextCreated`).
    ///
    /// Inert: the name and the aux data a host passes describe the context to a
    /// debugger, and there is none. A runtime makes this call whether or not one
    /// ever connects, which is why it must not refuse.
    pub fn context_created(
        &self,
        context: Local<Context>,
        context_group_id: i32,
        human_readable_name: StringView,
        aux_data: StringView,
    ) {
        let _ = (context, context_group_id, human_readable_name, aux_data);
    }

    /// Tell the inspector a context is gone
    /// (`v8::inspector::V8Inspector::contextDestroyed`).
    ///
    /// Inert, for the same reason as [`context_created`](Self::context_created).
    pub fn context_destroyed(&self, context: Local<Context>) {
        let _ = context;
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

    /// Report an exception to the inspector
    /// (`v8::inspector::V8Inspector::exceptionThrown`).
    ///
    /// Inert: nothing is reported, and the exception id V8 would answer is `0`.
    /// The runtime's own handling does not depend on it — `deno_core` calls this
    /// while dispatching an uncaught exception and then terminates the script
    /// itself (`libs/core/error.rs`).
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
            message,
            exception,
            detailed_message,
            url,
            line_number,
            column_number,
            stack_trace,
            script_id,
        );
        0
    }
}

/// A front end's connection to the inspector
/// (`v8::inspector::V8InspectorSession`).
///
/// No value of this type can be obtained: [`V8Inspector::connect`] refuses.
pub struct V8InspectorSession {
    /// The channel, held as the crate we stand in for holds it: for the
    /// session's lifetime, which never begins here.
    _channel: Channel,
}

impl V8InspectorSession {
    /// Whether the protocol has a handler for `method`
    /// (`v8::inspector::V8InspectorSession::canDispatchMethod`).
    ///
    /// Answers `false` for everything, which is the truth: there is no protocol
    /// and no handler.
    pub fn can_dispatch_method(method: StringView) -> bool {
        let _ = method;
        false
    }

    /// Dispatch a protocol message (`v8::inspector::V8InspectorSession::dispatchProtocolMessage`).
    ///
    /// # Panics
    ///
    /// Always. Only reachable through a connected session, which cannot exist.
    pub fn dispatch_protocol_message(&self, message: StringView) {
        let _ = message;
        panic!("v8::inspector::V8InspectorSession::dispatch_protocol_message: {NO_INSPECTOR}")
    }

    /// Ask to pause at the next statement
    /// (`v8::inspector::V8InspectorSession::schedulePauseOnNextStatement`).
    ///
    /// # Panics
    ///
    /// Always, for the same reason as
    /// [`dispatch_protocol_message`](Self::dispatch_protocol_message).
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
    /// Always, for the same reason as
    /// [`dispatch_protocol_message`](Self::dispatch_protocol_message).
    pub fn cancel_pause_on_next_statement(&self) {
        panic!("v8::inspector::V8InspectorSession::cancel_pause_on_next_statement: {NO_INSPECTOR}")
    }
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

    /// The context lifecycle is inert: a runtime tells its inspector about every
    /// realm it makes, whether or not a debugger ever connects.
    #[test]
    fn telling_the_inspector_about_a_context_is_inert() {
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
        inspector.context_destroyed(context);
    }

    /// The exception path is inert too: the runtime reports an uncaught
    /// exception on its way to terminating the script, so both calls have to
    /// answer — an empty trace to hand back, and the id `0`.
    #[test]
    fn the_inspectors_exception_path_is_inert() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, isolate);
        let context = crate::Context::new(scope, Default::default());
        let client = V8InspectorClient::new(Box::new(Noop));
        let inspector = V8Inspector::create(scope, client);

        let trace = inspector.create_stack_trace(None);
        assert!(!trace.is_null(), "an empty trace, not a null one");
        let thrown: Local<'_, Value> = crate::undefined(scope).into();
        assert_eq!(
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
            "nothing is reported, and the id is the crate's zero"
        );
    }

    /// The tier's refusing half: a host that attaches a debugger still meets the
    /// wall at once, which is the module's own "failing loudly beats a silent
    /// session" kept where it is still true.
    #[test]
    #[should_panic(expected = "Slag has no inspector")]
    fn connecting_a_debugger_refuses_with_the_reason() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let scope, isolate);
        let client = V8InspectorClient::new(Box::new(Noop));
        let inspector = V8Inspector::create(scope, client);
        inspector.connect(
            1,
            Channel::new(Box::new(NoopChannel)),
            StringView::empty(),
            V8InspectorClientTrustLevel::FullyTrusted,
        );
    }

    struct Noop;

    impl V8InspectorClientImpl for Noop {}

    struct NoopChannel;

    impl ChannelImpl for NoopChannel {
        fn send_response(&self, _call_id: i32, _message: UniquePtr<StringBuffer>) {}

        fn send_notification(&self, _message: UniquePtr<StringBuffer>) {}

        fn flush_protocol_notifications(&self) {}
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
