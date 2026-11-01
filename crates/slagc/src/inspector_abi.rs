//! The protocol session the facade's `v8-inspector.h` drives.
//!
//! `crates/inspector` owns the Chrome DevTools Protocol session: script ids,
//! breakpoint resolution, `Debugger.paused` and its call frames. This module is
//! the C surface over it and the place the host's channel meets the session:
//! every entry point answers the protocol messages it produced as an owned
//! list, which the facade sends through the `V8Inspector::Channel` it was
//! handed.
//!
//! The session's state lives on the context rather than behind an opaque
//! handle, because compiling a script has to register it with the session: V8
//! reports `Debugger.scriptParsed` when the script is compiled, and
//! `Script::Compile` is the only place that knows the text and the origin it
//! was compiled with.
//!
//! The messages a call produces keep the session's order: for a dispatch, the
//! events first and the response to the command last, so a transport answers
//! with the last message and notifies with the rest.

use slag::{ErrorKind, JsError};

use crate::{SlagContext, record, reentrant, text};

/// One protocol message as it crosses to C++: a JSON text the session produced,
/// owned by the [`SlagMessages`] that listed it.
#[repr(C)]
pub struct SlagProtocolMessage {
    pub ptr: *mut u8,
    pub len: usize,
}

/// The protocol messages one call produced, in the session's delivery order.
/// Null items with a zero length is an empty answer; free with
/// [`slag_messages_free`].
#[repr(C)]
pub struct SlagMessages {
    pub items: *mut SlagProtocolMessage,
    pub len: usize,
}

/// A context's protocol session and the events a compile queued for it.
pub(crate) struct InspectorState {
    pub(crate) session: inspector::Session,
    /// `Debugger.scriptParsed` events a compile produced. A compile cannot send
    /// a notification (it has no channel), so they wait for the next poll,
    /// which is the call that reaches the host's.
    pub(crate) pending: Vec<String>,
}

impl InspectorState {
    pub(crate) fn new() -> Self {
        Self {
            session: inspector::Session::new(),
            pending: Vec::new(),
        }
    }
}

/// Hand `messages` to the caller through `out`, each as its own owned span.
///
/// # Safety
/// `out` is either null or a `SlagMessages` the caller owns and has not freed.
unsafe fn answer(out: *mut SlagMessages, messages: Vec<String>) {
    let Some(out) = out.as_mut() else {
        return;
    };
    *out = SlagMessages {
        items: std::ptr::null_mut(),
        len: 0,
    };
    if messages.is_empty() {
        return;
    }
    let mut spans: Vec<SlagProtocolMessage> = messages
        .into_iter()
        .map(|message| {
            let mut bytes = message.into_bytes().into_boxed_slice();
            let span = SlagProtocolMessage {
                ptr: bytes.as_mut_ptr(),
                len: bytes.len(),
            };
            // The caller owns the bytes now: a `SlagMessages` reports its
            // spans by pointer, so the box is reclaimed only by
            // `slag_messages_free`.
            std::mem::forget(bytes);
            span
        })
        .collect();
    spans.shrink_to_fit();
    let len = spans.len();
    let items = spans.as_mut_ptr();
    std::mem::forget(spans);
    *out = SlagMessages { items, len };
}

/// Open a protocol session on `context` (`V8Inspector::connect`). Answers 0 on
/// success, 2 for a bad handle, and 3 when a session is already open: the state
/// is per context, and a context group with several contexts has no group-wide
/// session yet (see `.notes/chromium-facade.md` section 14.3).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_open(context: *mut SlagContext) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Ok(mut state) = context.inspector.try_borrow_mut() else {
        return 2;
    };
    if state.is_some() {
        return 3;
    }
    *state = Some(InspectorState::new());
    0
}

/// Close the session: a parked run resumes, the engine detaches, and the script
/// registry is dropped. Answers 0 on success (including when nothing was open),
/// 1 with a pending error, and 2 for a bad handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_close(context: *mut SlagContext) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Ok(mut state) = context.inspector.try_borrow_mut() else {
        return 1;
    };
    if state.take().is_none() {
        return 0;
    }
    drop(state);
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        return 1;
    };
    let mut status = 0;
    // A stop holds the run's activations. Closing the session must not leave
    // the host holding a parked run that nothing can resume or report, so the
    // run is continued before the debug points are dropped.
    let resumed = engine.paused().is_some().then(|| engine.resume_paused());
    if let Some(Err(error)) = resumed {
        record(context, error);
        status = 1;
    }
    if let Err(error) = engine.set_debug_attached(false) {
        if status == 0 {
            record(context, error);
        }
        status = 1;
    }
    status
}

/// Feed one protocol message to the session, answering the events it caused and
/// the response to the command. Answers 0 on success (a protocol-level error is
/// an ordinary response, not a failed call), 1 with a pending error, and 2 for
/// a bad handle or when no session is open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_dispatch(
    context: *mut SlagContext,
    message: *const u8,
    len: usize,
    out: *mut SlagMessages,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Ok(message) = text(message, len) else {
        return 2;
    };
    let Ok(mut guard) = context.inspector.try_borrow_mut() else {
        // The session is already borrowed: a channel callback reached back into
        // it, which the facade's `Channel` contract forbids.
        reentrant(context);
        return 1;
    };
    let Some(state) = guard.as_mut() else {
        return 2;
    };
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        reentrant(context);
        return 1;
    };
    let messages = state.session.dispatch(&mut engine, message);
    drop(engine);
    drop(guard);
    answer(out, messages);
    0
}

/// Drain what the run produced: the `Debugger.scriptParsed` events a compile
/// queued, then a `Debugger.paused` for a stop that has not been announced.
/// Every message is an event. Answers 0 on success, 1 with a pending error, and
/// 2 for a bad handle or when no session is open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_poll(
    context: *mut SlagContext,
    out: *mut SlagMessages,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Ok(mut guard) = context.inspector.try_borrow_mut() else {
        return 1;
    };
    let Some(state) = guard.as_mut() else {
        return 2;
    };
    let Ok(engine) = context.context.try_borrow() else {
        reentrant(context);
        return 1;
    };
    let mut messages: Vec<String> = state.pending.drain(..).collect();
    messages.extend(state.session.poll(&engine));
    drop(engine);
    drop(guard);
    answer(out, messages);
    0
}

/// Evaluate `source` in the session's context (`V8InspectorSession::evaluate`),
/// which is how a host injects the command line API it installs. Answers 0 with
/// the value through `out`, 1 with a pending error, and 2 for a bad handle or
/// when no session is open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_evaluate(
    context: *mut SlagContext,
    source: *const u8,
    len: usize,
    out: *mut u64,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Ok(source) = text(source, len) else {
        return 2;
    };
    match context.inspector.try_borrow() {
        Ok(state) if state.is_some() => {}
        _ => return 2,
    }
    let Ok(mut engine) = context.context.try_borrow_mut() else {
        reentrant(context);
        return 1;
    };
    match engine.eval(source) {
        Ok(value) => {
            if let Some(out) = out.as_mut() {
                *out = value.to_bits();
            }
            0
        }
        Err(error) => {
            record(context, error);
            1
        }
    }
}

/// Whether the session answers `method` (`V8InspectorSession::canDispatchMethod`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_can_dispatch(method: *const u8, len: usize) -> i32 {
    let Ok(method) = text(method, len) else {
        return 0;
    };
    i32::from(inspector::Session::can_dispatch(method))
}

/// The command id `message` answers, or -1 when it names none: what a transport
/// needs to tell the answer to a command from an event.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_call_id(message: *const u8, len: usize) -> i64 {
    let Ok(message) = text(message, len) else {
        return -1;
    };
    inspector::Session::call_id(message).unwrap_or(-1)
}

/// Free the messages a call answered and the array that listed them.
///
/// # Safety
/// `messages` is either null or a list this ABI produced and has not freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_messages_free(messages: *mut SlagMessages) {
    let Some(messages) = messages.as_mut() else {
        return;
    };
    if !messages.items.is_null() {
        let spans = std::slice::from_raw_parts(messages.items, messages.len);
        for span in spans {
            if !span.ptr.is_null() {
                // The same layout `answer` leaked: a boxed byte slice.
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    span.ptr, span.len,
                )));
            }
        }
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            messages.items,
            messages.len,
        )));
    }
    *messages = SlagMessages {
        items: std::ptr::null_mut(),
        len: 0,
    };
}

/// Register `source` with the session under `name`, the way compiling it does.
///
/// The facade compiles through [`slag_compile`], which calls this itself, so
/// this entry point is for a host that has text it did not compile here (a
/// script it loaded as a module) and still wants it breakable. Answers 0 on
/// success, 1 with a pending error, and 2 for a bad handle or no session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_add_script(
    context: *mut SlagContext,
    name: *const u8,
    name_len: usize,
    source: *const u8,
    source_len: usize,
    out: *mut SlagMessages,
) -> i32 {
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Ok(name) = text(name, name_len) else {
        return 2;
    };
    let Ok(source) = text(source, source_len) else {
        return 2;
    };
    let Ok(mut guard) = context.inspector.try_borrow_mut() else {
        return 2;
    };
    let Some(state) = guard.as_mut() else {
        return 2;
    };
    let Ok(engine) = context.context.try_borrow() else {
        reentrant(context);
        return 1;
    };
    match state.session.add_script(&engine, name, source) {
        Ok((_id, events)) => {
            drop(engine);
            drop(guard);
            answer(out, events);
            0
        }
        Err(message) => {
            drop(engine);
            drop(guard);
            record(context, JsError::new(ErrorKind::SyntaxError, message));
            1
        }
    }
}
