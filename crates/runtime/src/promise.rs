//! Promise abstract operations (spec 27.2.1) and the Promise Record
//! machinery. The full `%Promise%` builtin (constructor, prototype methods,
//! and statics) lives in `builtins/promise.rs`; this module holds the
//! state types and the abstract operations both the builtins and the
//! async/generator machinery drive.

use std::cell::RefCell;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::heap::{GcAny, Trace};
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_callable, is_constructor};

use crate::agent::Agent;
use crate::job::{JobCallback, host_make_job_callback};

/// Which promise hook is running (`v8::PromiseHookType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromiseHookKind {
    /// A promise was created.
    Init,
    /// A reaction job's handler is about to run.
    Before,
    /// That handler returned.
    After,
    /// A promise settled.
    Resolve,
}

/// Run the current context's promise hook for `kind`, if it installed one
/// (`v8::Context::SetPromiseHooks`; V8's `NativeContext::RunPromiseHook`,
/// `v8/src/objects/contexts.cc:764`).
///
/// V8's shape, reproduced: the slot is the *current* context's, an unset slot
/// runs nothing, the receiver is the global object, and an init gets two
/// arguments — the promise and its parent — where the other kinds get one. A
/// hook that throws fails the operation that ran it, which is what the runtime
/// functions that fire these answer in V8.
///
/// The parent of an init is `undefined` here: V8 passes the promise whose
/// reaction created this one where it knows it, and this engine does not track
/// which reaction a promise came out of (`.notes/embedding.md` §9).
pub fn run_promise_hook(
    agent: &mut Agent,
    kind: PromiseHookKind,
    promise: &Value,
    parent: Option<Value>,
) -> Result<(), JsError> {
    // The slots are the *current* context's, as they are in V8
    // (`NativeContext::RunPromiseHook` reads the native context). A job runs
    // with no execution context of its own and the drain enters the realm the
    // job was queued for, which is what makes the before and after pair
    // reachable.
    let Ok(realm) = agent.current_realm() else {
        return Ok(());
    };
    let hook = {
        let hooks = realm.promise_hooks.borrow();
        match kind {
            PromiseHookKind::Init => hooks.init,
            PromiseHookKind::Before => hooks.before,
            PromiseHookKind::After => hooks.after,
            PromiseHookKind::Resolve => hooks.resolve,
        }
    };
    let Some(hook) = hook else {
        return Ok(());
    };
    let receiver = Value::Object(realm.global_object);
    let args = match kind {
        // V8's `argc` is 2 for an init and 1 for every other kind, so an init
        // is handed the parent even when it is `undefined`
        // (`NativeContext::RunPromiseHook`).
        PromiseHookKind::Init => vec![*promise, parent.unwrap_or(Value::Undefined)],
        _ => vec![*promise],
    };
    crate::function::call(agent, &hook, receiver, &args)?;
    Ok(())
}

/// [[PromiseState]] plus the reaction lists (spec 27.2.1.3).
pub enum PromiseState {
    Pending {
        fulfill_reactions: Vec<PromiseReaction>,
        reject_reactions: Vec<PromiseReaction>,
    },
    Fulfilled(Value),
    Rejected(Value),
}

impl std::fmt::Debug for PromiseState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromiseState::Pending { .. } => f.write_str("pending"),
            PromiseState::Fulfilled(_) => f.write_str("fulfilled"),
            PromiseState::Rejected(_) => f.write_str("rejected"),
        }
    }
}

impl Trace for PromiseState {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        match self {
            PromiseState::Pending {
                fulfill_reactions,
                reject_reactions,
            } => {
                fulfill_reactions.trace(visit);
                reject_reactions.trace(visit);
            }
            PromiseState::Fulfilled(value) | PromiseState::Rejected(value) => value.trace(visit),
        }
    }
}

/// The agent-side Promise Record: the [[PromiseState]] plus the reaction
/// lists.
#[derive(Debug)]
pub struct PromiseData {
    pub state: PromiseState,
    /// [[IsHandled]]: whether any reaction was attached (unhandled-rejection
    /// tracking).
    pub is_handled: bool,
}

impl Trace for PromiseData {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.state.trace(visit);
    }
}

/// A resolving function record (spec 27.2.1.3.1): which promise it resolves
/// and whether it has already fired.
#[derive(Debug)]
pub struct ResolverData {
    pub promise: Value,
    /// The [[AlreadyResolved]] flag, shared by the resolve and reject
    /// functions of one capability (spec 27.2.1.3.1).
    pub already_resolved: std::rc::Rc<std::cell::Cell<bool>>,
    pub is_reject: bool,
}

impl Trace for ResolverData {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.promise.trace(visit);
    }
}

/// A PromiseReaction Record (spec 27.2.1.5).
#[derive(Clone)]
pub struct PromiseReaction {
    /// The result capability of the `then` that created this reaction; `None`
    /// for internal reactions (async/await resumptions) that only run the
    /// handler.
    pub capability: Option<PromiseCapability>,
    /// [[Type]]: which reaction list the reaction came from.
    pub kind: ReactionKind,
    /// [[Handler]]; empty for non-callable arguments (the default behavior).
    pub handler: Option<JobCallback>,
}

impl std::fmt::Debug for PromiseReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromiseReaction")
            .field("capability", &self.capability.is_some())
            .field("kind", &self.kind)
            .finish()
    }
}

impl Trace for PromiseReaction {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.capability.trace(visit);
        // JobCallback (job.rs) has no Trace impl of its own; trace its Value
        // fields directly.
        if let Some(handler) = &self.handler {
            handler.callback.trace(visit);
            handler.host_defined.trace(visit);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKind {
    Fulfill,
    Reject,
}

/// A PromiseCapability Record (spec 27.2.1.1): the promise plus its
/// resolving functions.
#[derive(Debug, Clone)]
pub struct PromiseCapability {
    pub promise: Value,
    pub resolve: Value,
    pub reject: Value,
}

impl Trace for PromiseCapability {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.promise.trace(visit);
        self.resolve.trace(visit);
        self.reject.trace(visit);
    }
}

/// IsPromise (spec 27.2.1.4): the value is an object with a Promise Record.
pub fn is_promise(agent: &Agent, value: &Value) -> bool {
    let ValueKind::Object(obj) = value.kind() else {
        return false;
    };
    agent.promises.contains_key(&obj.id())
}

/// NewPromiseCapability (spec 27.2.1.5): construct `constructor` with an
/// executor that captures the resolving functions.
pub fn new_promise_capability(
    agent: &mut Agent,
    constructor: &Value,
) -> Result<PromiseCapability, JsError> {
    if !is_constructor(constructor) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise constructor is not a constructor".into(),
        ));
    }
    let captured: Rc<RefCell<Option<(Value, Value)>>> = Rc::new(RefCell::new(None));
    let function_proto = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get("%Function.prototype%"))
        .and_then(|v| crate::context::as_object(&v));
    let executor = Function::create_builtin(
        Some(JsString::from_utf8("")),
        2,
        Box::new({
            let captured = captured.clone();
            move |_, args| {
                let mut slot = captured.borrow_mut();
                // GetCapabilitiesExecutor (spec 27.2.1.5.1 steps 1-4): a
                // second call throws once a resolve/reject was captured; a
                // prior (undefined, undefined) call leaves the slot free.
                if let Some((resolve, reject)) = &*slot
                    && (!matches!(resolve.kind(), ValueKind::Undefined)
                        || !matches!(reject.kind(), ValueKind::Undefined))
                {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Promise executor called twice".into(),
                    ));
                }
                let resolve = args.first().cloned().unwrap_or(Value::Undefined);
                let reject = args.get(1).cloned().unwrap_or(Value::Undefined);
                *slot = Some((resolve, reject));
                Ok(Value::Undefined)
            }
        }),
        None,
        function_proto,
    )?;
    // GC-2: the executor writes the resolving functions into the native
    // `captured` cell while the (user-provided) constructor body still runs
    // — the cell is a heap buffer the conservative stack scan cannot see,
    // so a per-allocation `--gc-stress` collection fired by the rest of the
    // body would sweep them. Suppress the construct window; the values
    // move onto the stack (scan-visible) once `captured` is read back.
    let _stress = crate::ir::StressSuppress::new();
    let promise = crate::function::construct(
        agent,
        constructor,
        &[Value::Function(executor)],
        constructor,
    )?;
    let Some((resolve, reject)) = *captured.borrow() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise executor did not call resolve or reject".into(),
        ));
    };
    if !is_callable(&resolve) || !is_callable(&reject) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise resolve and reject functions must be callable".into(),
        ));
    }
    Ok(PromiseCapability {
        promise,
        resolve,
        reject,
    })
}

/// CreateResolvingFunctions (spec 27.2.1.3.1): two built-in functions whose
/// identity the call dispatcher routes back here.
pub fn create_resolving_functions(agent: &mut Agent, promise: &Value) -> (Value, Value) {
    let function_proto = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get("%Function.prototype%"))
        .and_then(|v| crate::context::as_object(&v));
    // The promise resolving functions are anonymous (spec 27.2.1.3.1): the
    // `name` property is the empty string. The two share one [[AlreadyResolved]]
    // flag, so resolve-then-throw or reject-then-resolve is a no-op.
    let already_resolved = std::rc::Rc::new(std::cell::Cell::new(false));
    let mut make = |is_reject: bool, name: &str| -> Value {
        let resolver = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            1,
            Box::new(|_, _| {
                Err(JsError::new(
                    ErrorKind::TypeError,
                    "Promise resolving functions must be called through the agent".into(),
                ))
            }),
            None,
            function_proto,
        )
        .expect("builtin creation cannot fail");
        agent.promise_resolvers.insert(
            resolver.id(),
            Rc::new(RefCell::new(ResolverData {
                promise: *promise,
                already_resolved: already_resolved.clone(),
                is_reject,
            })),
        );
        Value::Function(resolver)
    };
    (make(false, ""), make(true, ""))
}

/// The Promise Resolve Function algorithm (spec 27.2.1.3.2): resolve
/// `promise` with `resolution`, deferring thenables through a job.
pub fn resolve_promise(
    agent: &mut Agent,
    promise: &Value,
    resolution: Value,
) -> Result<(), JsError> {
    run_promise_hook(agent, PromiseHookKind::Resolve, promise, None)?;
    if crux::ops::same_value(&resolution, promise) {
        // spec 27.2.1.3.2 step 6: reject with a fresh TypeError object (the
        // constructor check in the resolve-*-self fixtures needs a real
        // TypeError, not a string).
        let error = crate::builtins::error::to_throwable(
            agent,
            &JsError::new(
                ErrorKind::TypeError,
                "Chaining cycle detected for promise".into(),
            ),
        )?;
        return reject_promise(agent, promise, error);
    }
    let object = match resolution.kind() {
        ValueKind::Object(obj) => Some(obj),
        ValueKind::Function(fun) => fun.object.handle(),
        _ => None,
    };
    let Some(_object) = object else {
        return fulfill_promise(agent, promise, resolution);
    };
    let then =
        crate::context::get_property(agent, &resolution, &JsString::from_utf8("then"), resolution);
    let then = match then {
        Ok(then) => then,
        Err(error) => {
            let rejection = error_value(agent, &error);
            return reject_promise(agent, promise, rejection);
        }
    };
    if !is_callable(&then) {
        return fulfill_promise(agent, promise, resolution);
    }
    // Enqueue a NewPromiseResolveThenableJob (spec 27.2.1.8).
    enqueue_resolve_thenable_job(agent, *promise, resolution, then)
}

/// The Promise Reject Function algorithm: reject `promise` with `reason`.
pub fn reject_promise(agent: &mut Agent, promise: &Value, reason: Value) -> Result<(), JsError> {
    let ValueKind::Object(obj) = promise.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "reject called on a non-object promise".into(),
        ));
    };
    let Some(data) = agent.promises.get(&obj.id()) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "reject called on a non-promise object".into(),
        ));
    };
    let (reactions, was_handled) = {
        let mut data = data.borrow_mut();
        let PromiseState::Pending {
            reject_reactions, ..
        } = &mut data.state
        else {
            return Ok(());
        };
        (std::mem::take(reject_reactions), data.is_handled)
    };
    data.borrow_mut().state = PromiseState::Rejected(reason);
    run_promise_hook(agent, PromiseHookKind::Resolve, promise, None)?;
    if !was_handled {
        crate::host::promise_rejection_tracker(agent, promise, Some(&reason), false)?;
    }
    for reaction in reactions {
        enqueue_reaction_job(agent, *promise, reaction, reason);
    }
    Ok(())
}

/// FulfillPromise (spec 27.2.1.4): settle with a value and run the
/// fulfill reactions.
pub fn fulfill_promise(agent: &mut Agent, promise: &Value, value: Value) -> Result<(), JsError> {
    let ValueKind::Object(obj) = promise.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "fulfill called on a non-object promise".into(),
        ));
    };
    let Some(data) = agent.promises.get(&obj.id()) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "fulfill called on a non-promise object".into(),
        ));
    };
    let reactions = {
        let mut data = data.borrow_mut();
        let PromiseState::Pending {
            fulfill_reactions, ..
        } = &mut data.state
        else {
            return Ok(());
        };
        std::mem::take(fulfill_reactions)
    };
    data.borrow_mut().state = PromiseState::Fulfilled(value);
    run_promise_hook(agent, PromiseHookKind::Resolve, promise, None)?;
    for reaction in reactions {
        enqueue_reaction_job(agent, *promise, reaction, value);
    }
    Ok(())
}

/// PerformPromiseThen (spec 27.2.1.7): attach reactions to `promise` and
/// return the result capability's promise (or *undefined*).
#[allow(clippy::too_many_arguments)]
pub fn perform_promise_then(
    agent: &mut Agent,
    promise: &Value,
    on_fulfilled: Option<Value>,
    on_rejected: Option<Value>,
    result_capability: Option<PromiseCapability>,
) -> Result<Value, JsError> {
    let result_promise = result_capability.as_ref().map(|c| c.promise);
    let on_fulfilled = on_fulfilled.filter(is_callable);
    let on_rejected = on_rejected.filter(is_callable);
    let fulfill_reaction = PromiseReaction {
        capability: result_capability.clone(),
        kind: ReactionKind::Fulfill,
        handler: on_fulfilled.map(host_make_job_callback),
    };
    let reject_reaction = PromiseReaction {
        capability: result_capability,
        kind: ReactionKind::Reject,
        handler: on_rejected.map(host_make_job_callback),
    };
    let ValueKind::Object(obj) = promise.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "promise is not an object".into(),
        ));
    };
    let id = obj.id();
    let promise_again = *promise;
    // Attach the reactions, or take the settled value; the borrow ends before
    // jobs are enqueued (they need `&mut agent`).
    let outcome = {
        let data = agent
            .promises
            .get(&id)
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "promise is not a promise".into()))?;
        let mut data = data.borrow_mut();
        match &mut data.state {
            PromiseState::Pending {
                fulfill_reactions,
                reject_reactions,
            } => {
                fulfill_reactions.push(fulfill_reaction.clone());
                reject_reactions.push(reject_reaction.clone());
                None
            }
            PromiseState::Fulfilled(value) => Some((ReactionKind::Fulfill, *value)),
            PromiseState::Rejected(value) => Some((ReactionKind::Reject, *value)),
        }
    };
    if let Some((kind, value)) = outcome {
        let reaction = match kind {
            ReactionKind::Fulfill => fulfill_reaction,
            ReactionKind::Reject => reject_reaction,
        };
        enqueue_reaction_job(agent, promise_again, reaction, value);
    }
    // Step 7: mark the promise handled once a reaction is attached.
    let was_handled = agent.promises[&id].borrow().is_handled;
    if !was_handled {
        agent.promises[&id].borrow_mut().is_handled = true;
        let reason = match &agent.promises[&id].borrow().state {
            PromiseState::Rejected(value) => Some(*value),
            _ => None,
        };
        crate::host::promise_rejection_tracker(agent, &promise_again, reason.as_ref(), true)?;
    }
    // The result promise, per the spec's step 9.
    Ok(result_promise.unwrap_or(Value::Undefined))
}

/// PromiseResolve (spec 27.2.4.5.1): the static `Promise.resolve` core.
pub fn promise_resolve(agent: &mut Agent, constructor: &Value, x: Value) -> Result<Value, JsError> {
    if is_promise(agent, &x) {
        let ctor = crate::context::get_property(agent, &x, &JsString::from_utf8("constructor"), x)?;
        if crux::ops::same_value(&ctor, constructor) {
            return Ok(x);
        }
    }
    let capability = new_promise_capability(agent, constructor)?;
    crate::function::call(agent, &capability.resolve, Value::Undefined, &[x])?;
    Ok(capability.promise)
}

/// The value a rejected promise receives: the thrown language value, or a
/// TypeError for an internal error (Phase 8 binds real Error objects).
pub fn error_value(agent: &mut Agent, error: &JsError) -> Value {
    if let Some(value) = &error.value {
        return *value;
    }
    // Engine errors reject with a real Error object (spec ch. 17); the
    // message string is the fallback until the built-ins are installed.
    crate::builtins::error::to_throwable(agent, error).unwrap_or_else(|_| {
        Value::String(Handle::new(JsString::from_utf8(&format!(
            "{}: {}",
            kind_name(error.kind),
            error.message
        ))))
    })
}

fn kind_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Error => "Error",
        ErrorKind::TypeError => "TypeError",
        ErrorKind::RangeError => "RangeError",
        ErrorKind::ReferenceError => "ReferenceError",
        ErrorKind::SyntaxError => "SyntaxError",
        ErrorKind::EvalError => "EvalError",
        ErrorKind::UriError => "URIError",
    }
}

/// NewPromiseReactionJob (spec 27.2.1.6): run the reaction's handler with the
/// settled value and settle the reaction's capability.
///
/// `promise` is the promise whose reaction this is, which is what V8's before
/// and after hooks are handed (`OnPromiseBefore`/`OnPromiseAfter`).
fn enqueue_reaction_job(
    agent: &mut Agent,
    promise: Value,
    reaction: PromiseReaction,
    argument: Value,
) {
    let realm = agent.current_realm().ok();
    // V8's `TryGetCurrentTaskPromise` is computed before the job runs, from the
    // reaction this job carries: an async chain walk starts at this promise.
    let task_promise = reaction_task_promise(agent, &reaction);
    agent.enqueue_promise_job(realm, move |agent| {
        let saved = agent.current_task_promise;
        agent.current_task_promise = task_promise;
        let result = run_reaction_job(agent, promise, &reaction, argument);
        agent.current_task_promise = saved;
        result
    });
}

/// The promise the running reaction job's task belongs to
/// (`TryGetCurrentTaskPromise`, `v8/src/execution/isolate.cc:1453`). A job whose
/// handler *is* an await continuation — the engine registers those in
/// `async_resume` — belongs to the suspended body's own promise; any other job
/// belongs to the promise the reaction's capability derived. `None` for a
/// reaction with neither, which is where V8's walk has no promise to start at.
fn reaction_task_promise(agent: &Agent, reaction: &PromiseReaction) -> Option<Value> {
    if let Some(callback) = &reaction.handler
        && let ValueKind::Function(function) = callback.callback.kind()
        && let Some(resume) = agent.async_resume.get(&function.id())
    {
        return Some(resume.state.borrow().promise);
    }
    reaction
        .capability
        .as_ref()
        .map(|capability| capability.promise)
}

/// One reaction job's work, so that the task promise around it is set and
/// restored in one place whatever the handler does.
fn run_reaction_job(
    agent: &mut Agent,
    promise: Value,
    reaction: &PromiseReaction,
    argument: Value,
) -> Result<Value, JsError> {
    run_promise_hook(agent, PromiseHookKind::Before, &promise, None)?;
    let handler_result = match &reaction.handler {
        Some(callback) => crate::job::host_call_job_callback_agent(
            agent,
            callback,
            Value::Undefined,
            std::slice::from_ref(&argument),
        ),
        None => match reaction.kind {
            ReactionKind::Fulfill => Ok(argument),
            ReactionKind::Reject => Err(JsError::new(
                ErrorKind::TypeError,
                "Unhandled promise rejection".into(),
            )
            .with_value(argument)),
        },
    };
    // V8 fires the after hook from the job's epilogue, whatever the handler
    // did — the settlement below is the job's own tail, not the handler's.
    run_promise_hook(agent, PromiseHookKind::After, &promise, None)?;
    if let Some(capability) = &reaction.capability {
        match handler_result {
            Ok(value) => {
                crate::function::call(agent, &capability.resolve, Value::Undefined, &[value])?;
            }
            Err(error) => {
                let rejection = error_value(agent, &error);
                crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            }
        }
    }
    Ok(Value::Undefined)
}

/// NewPromiseResolveThenableJob (spec 27.2.1.8): call the thenable's `then`
/// with fresh resolving functions for the promise.
fn enqueue_resolve_thenable_job(
    agent: &mut Agent,
    promise: Value,
    thenable: Value,
    then: Value,
) -> Result<(), JsError> {
    let realm = agent.current_realm().ok();
    let (resolve, reject) = create_resolving_functions(agent, &promise);
    agent.enqueue_promise_job(realm, move |agent| {
        let result = crate::function::call(agent, &then, thenable, &[resolve, reject]);
        match result {
            Ok(_) => Ok(Value::Undefined),
            Err(error) => {
                let rejection = error_value(agent, &error);
                crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                Ok(Value::Undefined)
            }
        }
    });
    Ok(())
}
