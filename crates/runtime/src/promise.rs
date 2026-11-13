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
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_callable, is_constructor};

use crate::agent::Agent;
use crate::job::host_make_job_callback;

/// The promise records live on the promise object's own `ObjectKind`
/// (`ObjectKind::Promise`), so their types are crux's: the collector traces a
/// promise's reactions from the object that owns them, and there is no
/// identity-keyed table to reap.
pub use crux::promise::{
    PromiseCapability, PromiseData, PromiseReaction, PromiseState, ReactionKind,
};

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

/// [[IsHandled]]/state accessors are the record's own fields.
///
/// The record is the promise's, so reading it is one `kind` match.
fn promise_record(
    promise: &Value,
) -> Option<Handle<crux::heap::GcCell<crux::promise::PromiseData>>> {
    let ValueKind::Object(obj) = promise.kind() else {
        return None;
    };
    match &obj.kind {
        crux::object::ObjectKind::Promise(data) => Some(*data),
        _ => None,
    }
}

/// IsPromise (spec 27.2.1.4): the value is an object with a Promise Record.
pub fn is_promise(value: &Value) -> bool {
    promise_record(value).is_some()
}

/// Settle `capability` fulfilled with `value`: the spec's
/// `Call(capability.[[Resolve]], undefined, «value»)` for an observable
/// capability, and the same algorithm called directly for an internal one
/// (which has no resolving function to call).
pub fn settle_capability(
    agent: &mut Agent,
    capability: &PromiseCapability,
    value: Value,
) -> Result<(), JsError> {
    if capability.resolve.is_undefined() {
        return resolve_promise(agent, &capability.promise, value);
    }
    crate::function::call(agent, &capability.resolve, Value::Undefined, &[value])?;
    Ok(())
}

/// Reject `capability` with `reason`, the mirror of [`settle_capability`].
pub fn reject_capability(
    agent: &mut Agent,
    capability: &PromiseCapability,
    reason: Value,
) -> Result<(), JsError> {
    if capability.reject.is_undefined() {
        return reject_promise(agent, &capability.promise, reason);
    }
    crate::function::call(agent, &capability.reject, Value::Undefined, &[reason])?;
    Ok(())
}

/// The promise object of spec 27.2.4.1 steps 3-9 *without* its resolving
/// functions: for an internal caller whose executor is never user code (the
/// stock `%Promise%` path), the two resolvers cannot be observed by anyone, so
/// the settle is direct and the two closures are not built at all.
pub(crate) fn create_promise_object_bare(
    agent: &mut Agent,
    new_target: &Value,
) -> Result<Value, JsError> {
    let proto = crate::builtins::promise::get_prototype_from_constructor(
        agent,
        new_target,
        crate::builtins::promise::PROMISE_PROTO,
    )?;
    let (promise, _data) = crux::object::JsObject::promise_object_create(Some(proto));
    let promise_value = Value::Object(promise);
    run_promise_hook(agent, PromiseHookKind::Init, &promise_value, None)?;
    Ok(promise_value)
}

/// The promise object of spec 27.2.4.1 steps 3-9, without the executor: the
/// prototype, the object, its two resolving functions, its state record and the
/// init hook.
///
/// Shared by `promise_constructor` and the capability fast path below, so the
/// two cannot drift.
pub(crate) fn create_promise_object(
    agent: &mut Agent,
    new_target: &Value,
) -> Result<(Value, Value, Value), JsError> {
    let proto = crate::builtins::promise::get_prototype_from_constructor(
        agent,
        new_target,
        crate::builtins::promise::PROMISE_PROTO,
    )?;
    let (promise, data) = crux::object::JsObject::promise_object_create(Some(proto));
    let promise_value = Value::Object(promise);
    let (resolve, reject) = create_resolving_functions(agent, data);
    run_promise_hook(agent, PromiseHookKind::Init, &promise_value, None)?;
    Ok((promise_value, resolve, reject))
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
    // NewPromiseCapability's executor is unobservable for the realm's own
    // `%Promise%` — it exists only to hand the resolving functions back to the
    // caller. Building it costs a general `function::construct` whose body
    // calls a freshly minted builtin closure, and a fresh id is one no
    // dispatch cache can ever warm: the module chain re-probes on every
    // capability. Measured 2026-11-09: 18.3 us of the 18.7 us a capability
    // took. Every hot caller (`Promise.resolve`/`reject`/`then`, the async
    // function return) passes the stock constructor, so they all take this.
    if let Ok(realm) = agent.current_realm()
        && let Some(stock) = realm.intrinsics.get(crate::builtins::promise::PROMISE)
        && crux::ops::same_value(&stock, constructor)
    {
        let (promise, resolve, reject) = create_promise_object(agent, constructor)?;
        return Ok(PromiseCapability {
            promise,
            resolve,
            reject,
        });
    }
    let captured: Rc<RefCell<Option<(Value, Value)>>> = Rc::new(RefCell::new(None));
    let function_proto = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.function_prototype("%Function.prototype%"))
        .and_then(|v| crate::context::as_object(&v));
    let executor = Function::create_builtin(
        Some(JsString::from_utf8("")),
        2,
        Box::new({
            let captured = captured.clone();
            move |_agent, _, args| {
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

/// NewPromiseCapability for a caller that will only *call* the resolving
/// functions (never hand them to user code or read them as values), on the
/// realm's own `%Promise%`: the same promise, with no closures built.
///
/// `None` for any other constructor — the caller then takes
/// [`new_promise_capability`] and gets the observable pair.
pub fn new_internal_capability(
    agent: &mut Agent,
    constructor: &Value,
) -> Result<Option<PromiseCapability>, JsError> {
    if !is_stock_promise(agent, constructor) {
        return Ok(None);
    }
    let promise = create_promise_object_bare(agent, constructor)?;
    Ok(Some(PromiseCapability {
        promise,
        resolve: Value::Undefined,
        reject: Value::Undefined,
    }))
}

/// CreateResolvingFunctions (spec 27.2.1.3.1): two built-in functions whose
/// identity the call dispatcher routes back here.
pub fn create_resolving_functions(
    agent: &mut Agent,
    data: Handle<crux::heap::GcCell<crux::promise::PromiseData>>,
) -> (Value, Value) {
    let function_proto = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.function_prototype("%Function.prototype%"))
        .and_then(|v| crate::context::as_object(&v));
    // The promise resolving functions are anonymous (spec 27.2.1.3.1): the
    // `name` property is the empty string. The two share one [[AlreadyResolved]]
    // flag, so resolve-then-throw or reject-then-resolve is a no-op. The flag
    // and the promise it resolves are the promise's own record, held by both
    // resolver functions, so a dispatch needs no side table and the pair costs
    // no box of its own.
    let flag = crux::function::ResolverFlag::new();
    let make = |is_reject: bool, name: &str| -> Value {
        let resolver = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            1,
            Box::new(|_, _, _| {
                Err(JsError::new(
                    ErrorKind::TypeError,
                    "Promise resolving functions must be called through the agent".into(),
                ))
            }),
            None,
            function_proto,
        )
        .expect("builtin creation cannot fail");
        resolver.set_resolver(flag, data, is_reject);
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
    let Some(data) = promise_record(promise) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "reject called on a non-promise object".into(),
        ));
    };
    let was_handled = data.borrow().is_handled.get();
    let reactions = {
        let mut data = data.borrow_mut();
        let PromiseState::Pending {
            reject_reactions, ..
        } = &mut data.state
        else {
            return Ok(());
        };
        std::mem::take(reject_reactions)
    };
    data.borrow_mut().state = PromiseState::Rejected(reason);
    // The record is a GC box now, so a settled value stored into an OLD
    // promise has to be remembered: nothing else roots the value.
    crux::heap::write_barrier_box(&*data);
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
    let Some(data) = promise_record(promise) else {
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
    crux::heap::write_barrier_box(&*data);
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
    let Some(data) = promise_record(promise) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "promise is not a promise".into(),
        ));
    };
    let promise_again = *promise;
    // Attach the reactions, or take the settled value; the borrow ends before
    // jobs are enqueued (they need `&mut agent`).
    let outcome = {
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
    if outcome.is_none() {
        // The reactions just pushed carry values (the capability, the handler),
        // and the record is a GC box: an old promise that gains a young
        // capability must be remembered or the minor sweeps it.
        crux::heap::write_barrier_box(&*data);
    }
    if let Some((kind, value)) = outcome {
        let reaction = match kind {
            ReactionKind::Fulfill => fulfill_reaction,
            ReactionKind::Reject => reject_reaction,
        };
        enqueue_reaction_job(agent, promise_again, reaction, value);
    }
    // Step 7: mark the promise handled once a reaction is attached.
    let (was_handled, rejected_with) = {
        let data = data.borrow();
        let rejected = match &data.state {
            PromiseState::Rejected(value) => Some(*value),
            _ => None,
        };
        (data.is_handled.get(), rejected)
    };
    if !was_handled {
        data.borrow_mut().is_handled.set(true);
        crate::host::promise_rejection_tracker(
            agent,
            &promise_again,
            rejected_with.as_ref(),
            true,
        )?;
    }
    // The result promise, per the spec's step 9.
    Ok(result_promise.unwrap_or(Value::Undefined))
}

/// Whether `constructor` is the realm's own `%Promise%` — the capability path
/// whose executor (`GetCapabilitiesExecutor`) is never user code.
fn is_stock_promise(agent: &Agent, constructor: &Value) -> bool {
    agent.current_realm().is_ok_and(|realm| {
        realm
            .intrinsics
            .get(crate::builtins::promise::PROMISE)
            .is_some_and(|stock| crux::ops::same_value(&stock, constructor))
    })
}

/// PromiseResolve (spec 27.2.4.5.1): the static `Promise.resolve` core.
pub fn promise_resolve(agent: &mut Agent, constructor: &Value, x: Value) -> Result<Value, JsError> {
    if is_promise(&x) {
        let ctor = crate::context::get_property(agent, &x, &JsString::from_utf8("constructor"), x)?;
        if crux::ops::same_value(&ctor, constructor) {
            return Ok(x);
        }
    }
    // The stock constructor's capability is internal: its resolving functions
    // exist only to be called here, so the promise is settled directly instead
    // of building the pair and dispatching through it (two builtin closures
    // plus a general call per `Promise.resolve`/await).
    if is_stock_promise(agent, constructor) {
        let promise = create_promise_object_bare(agent, constructor)?;
        resolve_promise(agent, &promise, x)?;
        return Ok(promise);
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
        let saved = agent.roots.current_task_promise.get();
        agent.roots.current_task_promise.set(task_promise);
        let result = run_reaction_job(agent, promise, &reaction, argument);
        agent.roots.current_task_promise.set(saved);
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
        && let Some(resume) = agent
            .roots
            .async_resume
            .borrow()
            .get(&function.id())
            .cloned()
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
            Ok(value) => settle_capability(agent, capability, value)?,
            Err(error) => {
                let rejection = error_value(agent, &error);
                reject_capability(agent, capability, rejection)?;
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
    let Some(data) = promise_record(&promise) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "promise is not a promise".into(),
        ));
    };
    let (resolve, reject) = create_resolving_functions(agent, data);
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
