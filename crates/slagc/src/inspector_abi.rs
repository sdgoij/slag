//! The protocol session the facade's `v8-inspector.h` drives.
//!
//! `crates/inspector` owns the Chrome DevTools Protocol session: script ids,
//! breakpoint resolution, `Debugger.paused` and its call frames. This module is
//! the C surface over it and the place the host's channel meets the session:
//! every entry point answers the protocol messages it produced as an owned
//! list, which the facade sends through the `V8Inspector::Channel` it was
//! handed.
//!
//! A [`Group`] is one protocol session and the contexts it debugs, which is
//! what a CDP session spans: `V8ContextInfo` names a group, a Blink group is a
//! frame, and each world of a frame is a context of its own. A context joins by
//! pointer and is given an execution context id, and it is removed before the
//! context is freed (`SlagContext::drop`), because the group holds pointers.
//! The group keeps compiling scripts registered with it: V8 reports
//! `Debugger.scriptParsed` when a script is compiled, and `Script::Compile` is
//! the only place that knows the text and the origin it was compiled with.
//!
//! The messages a call produces keep the session's order: for a dispatch, the
//! events first and the response to the command last, so a transport answers
//! with the last message and notifies with the rest.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use inspector::Target;
use slag::{Context, ErrorKind, JsError};

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

/// One context of a group.
pub(crate) struct Member {
    pub(crate) context: *mut SlagContext,
    /// The `executionContextId` the protocol names it by.
    id: u32,
}

/// A context group's session: what one `V8InspectorSession` is.
pub(crate) struct Group {
    /// The session, or `None` until the host connects and after it disconnects.
    session: RefCell<Option<inspector::Session>>,
    /// The contexts that joined, in join order.
    pub(crate) contexts: RefCell<Vec<Member>>,
    next_id: Cell<u32>,
    /// Protocol events a compile queued. A compile has no channel, so they wait
    /// for the next poll, which is the call that reaches the host's.
    pending: RefCell<Vec<String>>,
}

impl Group {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            session: RefCell::new(None),
            contexts: RefCell::new(Vec::new()),
            next_id: Cell::new(1),
            pending: RefCell::new(Vec::new()),
        })
    }

    /// The execution context id a member was given.
    fn execution_context_of(&self, context: *mut SlagContext) -> Option<u32> {
        let members = self.contexts.try_borrow().ok()?;
        members
            .iter()
            .find(|member| std::ptr::eq(member.context, context))
            .map(|member| member.id)
    }

    /// Register a script a member compiled, queueing the events the session owes
    /// its client. `Ok(None)` when there is nothing to announce: no member, no
    /// session, or a session that is inside another call.
    pub(crate) fn register_script(
        &self,
        context: *mut SlagContext,
        engine: &Context,
        name: &str,
        source: &str,
    ) -> Result<Option<u32>, String> {
        let Some(execution_context) = self.execution_context_of(context) else {
            return Ok(None);
        };
        // A compile can run inside a channel callback, where the session is
        // already borrowed; the script still compiles and runs, it is just not
        // announced.
        let Ok(mut session) = self.session.try_borrow_mut() else {
            return Ok(None);
        };
        let Some(session) = session.as_mut() else {
            return Ok(None);
        };
        let (id, events) = session.add_script_in(execution_context, engine, name, source)?;
        if let Ok(mut pending) = self.pending.try_borrow_mut() {
            pending.extend(events);
        }
        Ok(Some(id))
    }
}

/// The C handle on a group: shared with the contexts that joined it, so a
/// context compiling a script reaches the session without a second pointer.
pub struct SlagInspector {
    group: Rc<Group>,
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

/// Record the re-entrancy error on a member, for a call that found the session
/// already inside another one (a channel callback dispatching).
fn refuse_reentry(group: &Group) {
    let member = group
        .contexts
        .try_borrow()
        .ok()
        .and_then(|members| members.first().map(|member| member.context));
    // SAFETY: a member is a live context (see `with_targets`).
    if let Some(context) = member {
        unsafe { reentrant(&*context) };
    }
}

/// Run `f` with the group's targets, each engine context borrowed for the call.
///
/// Answers 1 with a pending error when the group or one of its engines is
/// already borrowed, which is what a channel callback reaching back into the
/// session would do.
fn with_targets<R>(group: &Group, f: impl FnOnce(&mut [Target<'_>]) -> R) -> Result<R, i32> {
    let Ok(members) = group.contexts.try_borrow() else {
        refuse_reentry(group);
        return Err(1);
    };
    let mut engines = Vec::with_capacity(members.len());
    for member in members.iter() {
        // SAFETY: a member is a live context. Freeing one removes it from its
        // group first (`SlagContext::drop`), so a member pointer is never stale.
        let context = unsafe { &*member.context };
        let Ok(engine) = context.context.try_borrow_mut() else {
            reentrant(context);
            return Err(1);
        };
        engines.push((member.id, engine));
    }
    let mut targets: Vec<Target<'_>> = engines
        .iter_mut()
        .map(|(id, engine)| Target::new(*id, engine))
        .collect();
    Ok(f(&mut targets))
}

/// Create a group session (`V8Inspector::create` for one group). Answers 0 on
/// success and 2 for a bad argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_new(out: *mut *mut SlagInspector) -> i32 {
    let Some(out) = out.as_mut() else {
        return 2;
    };
    *out = Box::into_raw(Box::new(SlagInspector {
        group: Group::new(),
    }));
    0
}

/// Free the handle. Every context leaves the group first, so a compile after
/// this point finds no session. A parked run is not resumed: `close` is the
/// call that does that, and a host frees the handle after it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_free(inspector: *mut SlagInspector) {
    if inspector.is_null() {
        return;
    }
    let inspector = Box::from_raw(inspector);
    let group = &inspector.group;
    if let Ok(members) = group.contexts.try_borrow() {
        for member in members.iter() {
            // SAFETY: members are live contexts until they leave, and leaving
            // takes the slot's own `Rc`, not this pointer.
            if let Ok(mut slot) = (&*member.context).group.try_borrow_mut() {
                *slot = None;
            }
        }
    }
    if let Ok(mut members) = group.contexts.try_borrow_mut() {
        members.clear();
    }
    if let Ok(mut session) = group.session.try_borrow_mut() {
        *session = None;
    }
}

/// Add a context to the group, answering the execution context id it was given.
/// Answers 0 on success, 2 for a bad handle, and 3 when the context already
/// belongs to a group: one engine's debug points have one session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_add_context(
    inspector: *mut SlagInspector,
    context: *mut SlagContext,
    out_id: *mut u32,
) -> i32 {
    let Some(inspector) = inspector.as_ref() else {
        return 2;
    };
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let group = &inspector.group;
    let Ok(joined) = context.group.try_borrow() else {
        return 2;
    };
    if joined.is_some() {
        return 3;
    }
    drop(joined);
    let Ok(mut members) = group.contexts.try_borrow_mut() else {
        return 2;
    };
    if members
        .iter()
        .any(|member| std::ptr::eq(member.context, context))
    {
        return 3;
    }
    let id = group.next_id.get();
    group.next_id.set(id + 1);
    members.push(Member {
        context: context as *const SlagContext as *mut SlagContext,
        id,
    });
    drop(members);
    if let Ok(mut slot) = context.group.try_borrow_mut() {
        *slot = Some(Rc::clone(group));
    }
    if let Some(out_id) = out_id.as_mut() {
        *out_id = id;
    }
    0
}

/// Remove a context from the group (`contextDestroyed`, or a context leaving).
/// Answers 0 on success, 2 for a bad handle, and 3 when it is not a member.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_remove_context(
    inspector: *mut SlagInspector,
    context: *mut SlagContext,
) -> i32 {
    let Some(inspector) = inspector.as_ref() else {
        return 2;
    };
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let group = &inspector.group;
    let Ok(mut members) = group.contexts.try_borrow_mut() else {
        return 2;
    };
    let before = members.len();
    members.retain(|member| !std::ptr::eq(member.context, context));
    let removed = members.len() != before;
    drop(members);
    if !removed {
        return 3;
    }
    if let Ok(mut slot) = context.group.try_borrow_mut() {
        *slot = None;
    }
    0
}

/// Connect: start a session over the group's contexts. Answers 0 on success
/// (including when one is already open), 1 with a pending error, and 2 for a
/// bad handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_open(inspector: *mut SlagInspector) -> i32 {
    let Some(inspector) = inspector.as_ref() else {
        return 2;
    };
    let group = &inspector.group;
    let Ok(mut session) = group.session.try_borrow_mut() else {
        refuse_reentry(group);
        return 1;
    };
    if session.is_none() {
        *session = Some(inspector::Session::new());
    }
    0
}

/// Disconnect: a parked run in any context continues, every context detaches,
/// and the script registry is dropped. Answers 0 on success (including when
/// nothing was open), 1 with a pending error, and 2 for a bad handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_close(inspector: *mut SlagInspector) -> i32 {
    let Some(inspector) = inspector.as_ref() else {
        return 2;
    };
    let group = &inspector.group;
    let Ok(mut session) = group.session.try_borrow_mut() else {
        refuse_reentry(group);
        return 1;
    };
    if session.take().is_none() {
        return 0;
    }
    drop(session);
    if let Ok(mut pending) = group.pending.try_borrow_mut() {
        pending.clear();
    }
    let outcome = with_targets(group, |targets| {
        let mut status = 0;
        for target in targets.iter_mut() {
            // A stop holds the run's activations. Closing the session must not
            // leave a host holding a parked run that nothing can resume or
            // report, so the run is continued before the points are dropped.
            let resumed = target
                .context
                .paused()
                .is_some()
                .then(|| target.context.resume_paused());
            if let Some(Err(error)) = resumed {
                record_engine(group, target.id, error);
                status = 1;
            }
            if let Err(error) = target.context.set_debug_attached(false) {
                record_engine(group, target.id, error);
                status = 1;
            }
        }
        status
    });
    outcome.unwrap_or_else(|code| code)
}

/// Record an engine error on the member `id` names, so the host reads it from
/// the context it happened in.
fn record_engine(group: &Group, id: u32, error: JsError) {
    let context = group.contexts.try_borrow().ok().and_then(|members| {
        members
            .iter()
            .find(|member| member.id == id)
            .map(|member| member.context)
    });
    // SAFETY: a member is a live context (see `with_targets`).
    if let Some(context) = context {
        unsafe { record(&*context, error) };
    }
}

/// Feed one protocol message to the session, answering the events it caused and
/// the response to the command. Answers 0 on success (a protocol-level error is
/// an ordinary response, not a failed call), 1 with a pending error, and 2 for
/// a bad handle or when no session is open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_dispatch(
    inspector: *mut SlagInspector,
    message: *const u8,
    len: usize,
    out: *mut SlagMessages,
) -> i32 {
    let Some(inspector) = inspector.as_ref() else {
        return 2;
    };
    let Ok(message) = text(message, len) else {
        return 2;
    };
    let group = &inspector.group;
    let Ok(mut guard) = group.session.try_borrow_mut() else {
        refuse_reentry(group);
        return 1;
    };
    let Some(session) = guard.as_mut() else {
        return 2;
    };
    let Ok(messages) = with_targets(group, |targets| session.dispatch_group(targets, message))
    else {
        return 1;
    };
    drop(guard);
    answer(out, messages);
    0
}

/// Continue a parked run in any context of the group
/// (`V8InspectorSession::resume`), answering the events the resume produced.
/// The command's own response is dropped: the client did not send it.
/// Answers 0 on success, 1 with a pending error, and 2 for a bad handle or when
/// no session is open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_resume(
    inspector: *mut SlagInspector,
    out: *mut SlagMessages,
) -> i32 {
    let Some(inspector) = inspector.as_ref() else {
        return 2;
    };
    let group = &inspector.group;
    let Ok(mut guard) = group.session.try_borrow_mut() else {
        refuse_reentry(group);
        return 1;
    };
    let Some(session) = guard.as_mut() else {
        return 2;
    };
    // A dispatch answers the events it caused and then its response; the last
    // message is that response, and a resume the client did not send has none
    // to report.
    let Ok(mut messages) = with_targets(group, |targets| {
        session.dispatch_group(targets, "{\"id\":0,\"method\":\"Debugger.resume\"}")
    }) else {
        return 1;
    };
    messages.pop();
    drop(guard);
    answer(out, messages);
    0
}

/// Drain what the runs produced: the `Debugger.scriptParsed` events a compile
/// queued, then a `Debugger.paused` for a stop that has not been announced.
/// Every message is an event. Answers 0 on success, 1 with a pending error, and
/// 2 for a bad handle or when no session is open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_poll(
    inspector: *mut SlagInspector,
    out: *mut SlagMessages,
) -> i32 {
    let Some(inspector) = inspector.as_ref() else {
        return 2;
    };
    let group = &inspector.group;
    let Ok(mut guard) = group.session.try_borrow_mut() else {
        refuse_reentry(group);
        return 1;
    };
    let Some(session) = guard.as_mut() else {
        return 2;
    };
    let Ok(stopped) = with_targets(group, |targets| session.poll_group(targets)) else {
        return 1;
    };
    let mut messages: Vec<String> = match group.pending.try_borrow_mut() {
        Ok(mut pending) => pending.drain(..).collect(),
        Err(_) => Vec::new(),
    };
    messages.extend(stopped);
    drop(guard);
    answer(out, messages);
    0
}

/// Evaluate `source` in `context` (`V8InspectorSession::evaluate`), which is how
/// a host injects the command line API it installs. Answers 0 with the value
/// through `out`, 1 with a pending error, and 2 for a bad handle or when the
/// context is not a member of a connected group.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_evaluate(
    inspector: *mut SlagInspector,
    context: *mut SlagContext,
    source: *const u8,
    len: usize,
    out: *mut u64,
) -> i32 {
    let Some(inspector) = inspector.as_ref() else {
        return 2;
    };
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Ok(source) = text(source, len) else {
        return 2;
    };
    let group = &inspector.group;
    if group
        .execution_context_of(context as *const SlagContext as *mut SlagContext)
        .is_none()
    {
        return 2;
    }
    match group.session.try_borrow() {
        Ok(session) if session.is_some() => {}
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

/// Register text the host did not compile through [`slag_compile`], which
/// registers the scripts it compiles itself. The events due are queued and
/// answered by the next [`slag_inspector_poll`]. Answers 0 on success, 1 with a
/// pending error (the body the debugger needs did not compile), and 2 for a bad
/// handle or a context that is not a member.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slag_inspector_add_script(
    inspector: *mut SlagInspector,
    context: *mut SlagContext,
    name: *const u8,
    name_len: usize,
    source: *const u8,
    source_len: usize,
) -> i32 {
    let Some(inspector) = inspector.as_ref() else {
        return 2;
    };
    let Some(context) = context.as_ref() else {
        return 2;
    };
    let Ok(name) = text(name, name_len) else {
        return 2;
    };
    let Ok(source) = text(source, source_len) else {
        return 2;
    };
    let Ok(engine) = context.context.try_borrow() else {
        reentrant(context);
        return 1;
    };
    let pointer = context as *const SlagContext as *mut SlagContext;
    match inspector
        .group
        .register_script(pointer, &engine, name, source)
    {
        Ok(Some(_)) => 0,
        Ok(None) => 2,
        Err(message) => {
            record(context, JsError::new(ErrorKind::SyntaxError, message));
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
/// needs to tell a response from an event.
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
