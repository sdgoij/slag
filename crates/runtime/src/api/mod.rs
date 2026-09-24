//! The V8-shaped embedding API: isolates, contexts, handles, templates,
//! exceptions, and host functions.
//!
//! This is the Rust foundation the drop-in `v8` crate (`crates/v8`) builds on,
//! through which a host that embeds Slag takes its V8-shaped surface. It mirrors the shape of the V8 embedder API —
//! an [`Isolate`] owns the heap/execution state, a [`Context`] is a realm on
//! an isolate, [`Local`]/[`Global`] are handles, [`FunctionTemplate`]/
//! [`ObjectTemplate`] create host functions and objects, and
//! [`TryCatch`]/[`Exception`] model pending exceptions.
//!
//! Divergences from V8, all deliberate:
//! - Values are `Rc`-backed: handles are always valid, handle scopes are
//!   advisory markers, and `Global` is a strong reference either way.
//! - One context per isolate (the agent owns its realm).
//! - Microtasks only run when [`Context::run_microtasks`] is called; the
//!   auto-drain behaviour of the classic `embed` API is not inherited.

mod context;
mod external;
mod handle;
mod heap;
mod json;
mod microtask;
mod module;
mod object;
mod promise;
mod script;
pub(crate) mod stack_trace;
mod template;
mod try_catch;
#[cfg(feature = "wasm")]
mod wasm;

pub use crate::agent::NearHeapLimitCallback;
pub use context::{Context, ContextScope};
pub use external::External;
pub use handle::{EscapableHandleScope, Global, HandleScope, Local, MaybeLocal};
pub use heap::HeapStatistics;
pub use json::Json;
pub use microtask::MicrotasksPolicy;
pub use module::{
    Module, ModuleImportPhase, ModuleRequest, ModuleStatus, SyntheticModuleEvaluationSteps,
};
pub use object::{Array, Object};
pub use promise::Promise;
pub use script::Script;
pub use stack_trace::StackFrame;
pub use template::{
    FunctionCallback, FunctionCallbackInfo, FunctionTemplate, ObjectTemplate, PropertyAttributes,
    ReturnSlot,
};
pub use try_catch::{Exception, TryCatch};
#[cfg(feature = "wasm")]
pub use wasm::{CompiledWasmModule, WasmModuleObject, WasmStreaming};

// The one place a host callback becomes a callable, so a function a template
// materializes and one a snapshot restores are the same shape — re-exported for
// the snapshot format, which restores the second kind.
pub(crate) use template::host_function;

use std::cell::RefCell;
use std::collections::HashMap;

use crux::error::JsError;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;

/// The isolate: the heap/execution state (an [`Agent`]), the pending
/// exception slot, and host data slots (v8::Isolate::SetData/GetData).
///
/// `Isolate::new` returns the isolate **boxed**: contexts and templates hold
/// raw pointers to it, so the heap allocation must stay put even if the
/// `Box` is moved. `repr(C)` keeps the agent at offset 0 so the TLS agent
/// pointer recorded by `crux::function::with_agent` doubles as the isolate
/// pointer (see [`Isolate::get_current`]).
#[repr(C)]
pub struct Isolate {
    pub(crate) agent: Agent,
    pub(crate) pending_exception: RefCell<Option<Value>>,
    pub(crate) data: RefCell<HashMap<u32, usize>>,
    /// When the queues drain without the host asking (v8::Isolate's
    /// SetMicrotasksPolicy). `Explicit` is the engine's own behaviour: nothing
    /// drains a job unless the host asks.
    pub(crate) microtasks_policy: std::cell::Cell<crate::api::MicrotasksPolicy>,
    /// How many embedder entries are on the stack. A re-entrant entry — a host
    /// callback calling back in — must not drain mid-callback, which is why the
    /// policy's `Auto` drains only when this reaches zero.
    pub(crate) entry_depth: std::cell::Cell<u32>,
}

impl Isolate {
    /// Create a fresh isolate: a bare agent with no realm. A [`Context`]
    /// must be created before any script can run (like V8: no context, no
    /// execution).
    ///
    /// The isolate is boxed so its address is stable: contexts, templates,
    /// and the TLS agent window hold raw pointers to it, and moving a `Box`
    /// moves the pointer, not the allocation.
    pub fn new() -> Box<Self> {
        Box::new(Self {
            agent: Agent::new(),
            pending_exception: RefCell::new(None),
            data: RefCell::new(HashMap::new()),
            microtasks_policy: std::cell::Cell::new(crate::api::MicrotasksPolicy::Explicit),
            entry_depth: std::cell::Cell::new(0),
        })
    }

    /// When the job queues drain without the host asking
    /// (v8::Isolate::GetMicrotasksPolicy).
    pub fn get_microtasks_policy(&self) -> crate::api::MicrotasksPolicy {
        self.microtasks_policy.get()
    }

    /// Set when the job queues drain without the host asking
    /// (v8::Isolate::SetMicrotasksPolicy).
    ///
    /// The crate we stand in for defaults to `Auto`; this engine defaults to
    /// `Explicit`, which is what it has always done, so that a host inherits no
    /// draining it did not ask for. A host that wants `Auto` says so.
    pub fn set_microtasks_policy(&mut self, policy: crate::api::MicrotasksPolicy) {
        self.microtasks_policy.set(policy);
    }

    /// The underlying agent (advanced use; the spec state lives here).
    pub fn agent(&mut self) -> &mut Agent {
        &mut self.agent
    }

    /// The exact source text of `function`'s definition, when the record keeps
    /// one (`Function.prototype.toString`, and the code cache a host asks a
    /// function for).
    ///
    /// `None` for a value that is not a function and for a callable the engine
    /// did not parse from source — a builtin's body is a builtin, and a
    /// synthesized function has no text of its own.
    pub fn function_source(&mut self, function: &Local) -> Option<String> {
        let ValueKind::Function(function) = function.value().kind() else {
            return None;
        };
        self.agent
            .ecma_functions
            .get(&function.id())
            .and_then(|data| data.source.as_ref())
            .map(|source| source.to_string_lossy())
    }

    /// The current isolate on this thread, or `None` outside an eval/call
    /// window. The agent is the first field (`repr(C)`), so the TLS agent
    /// pointer set by `crux::function::with_agent` and the isolate pointer
    /// coincide. Valid only while that window is on the stack.
    pub fn get_current() -> Option<*mut Isolate> {
        let agent = crux::function::current_agent();
        if agent.is_null() {
            None
        } else {
            Some(agent as *mut Isolate)
        }
    }

    /// The agent pointer at offset 0 (FFI-facing; valid while the isolate
    /// is alive).
    pub fn agent_ptr(&self) -> *mut Agent {
        self as *const Isolate as *const Agent as *mut Agent
    }

    /// Drain the job queues: promise (microtask), timeout, then generic
    /// jobs (v8::Isolate::RunMicrotasks, minus the platform hook).
    pub fn run_microtasks(&mut self) -> Result<(), JsError> {
        self.agent.run_jobs()
    }

    /// v8::Isolate::SetData/GetData: host data slots keyed by slot number.
    pub fn set_data(&self, slot: u32, data: usize) {
        self.data.borrow_mut().insert(slot, data);
    }

    /// The value of a host data slot, if set.
    pub fn get_data(&self, slot: u32) -> Option<usize> {
        self.data.borrow().get(&slot).copied()
    }

    /// Throw a language value: set the pending exception
    /// (v8::Isolate::ThrowException).
    pub fn throw_exception(&self, value: Value) {
        self.set_pending_exception(value);
    }

    /// Whether a pending exception is set.
    pub fn has_pending_exception(&self) -> bool {
        self.pending_exception.borrow().is_some()
    }

    /// The pending exception value, if set.
    pub fn pending_exception(&self) -> Option<Value> {
        *self.pending_exception.borrow()
    }

    pub fn set_pending_exception(&self, value: Value) {
        *self.pending_exception.borrow_mut() = Some(value);
    }

    pub fn take_pending_exception(&self) -> Option<Value> {
        self.pending_exception.borrow_mut().take()
    }

    /// Ask a running execution to stop (v8::Isolate::TerminateExecution).
    ///
    /// The request is observed at the engine's check points rather than
    /// interrupting anything: a loop's back edge, a compiled loop's probe, a
    /// call, and this API's own entries.
    pub fn terminate_execution(&self) {
        self.agent.request_termination();
    }

    /// Withdraw the request (v8::Isolate::CancelTerminateExecution), which is
    /// what lets the isolate run again.
    pub fn cancel_terminate_execution(&self) {
        self.agent.cancel_termination();
    }

    /// Whether a request is outstanding (v8::Isolate::IsExecutionTerminating).
    pub fn is_execution_terminating(&self) -> bool {
        self.agent.is_terminating()
    }

    /// The termination flag itself, for the handle that has to reach this
    /// isolate from another thread (`Isolate` is thread-local; the flag is not).
    pub fn termination_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.agent.termination_flag()
    }

    /// A host's heap size limit (`v8::CreateParams::heap_limits`), measured
    /// against the heap's live-box footprint. A `maximum` of 0 is V8's "no
    /// limit", which is what an isolate that never sets one has.
    pub fn set_heap_limits(&mut self, initial: usize, maximum: usize) {
        self.agent.set_heap_limits(initial, maximum);
    }

    /// The heap limit in force, if any
    /// (v8::Isolate::GetHeapStatistics's `heap_size_limit`).
    pub fn heap_limit(&self) -> Option<usize> {
        self.agent.heap_limit()
    }

    /// Install the callback a heap that reached its limit runs
    /// (`v8::Isolate::AddNearHeapLimitCallback`); `None` withdraws one.
    pub fn set_near_heap_limit_callback(
        &mut self,
        callback: Option<crate::agent::NearHeapLimitCallback>,
    ) {
        self.agent.set_near_heap_limit_callback(callback);
    }

    /// Withdraw the callback and set the limit to restore
    /// (`v8::Isolate::RemoveNearHeapLimitCallback`), where 0 leaves the limit
    /// alone.
    pub fn remove_near_heap_limit_callback(&mut self, heap_limit: usize) {
        self.agent.clear_near_heap_limit_callback(heap_limit);
    }
}

impl Default for Box<Isolate> {
    fn default() -> Self {
        Isolate::new()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    fn isolate() -> Box<Isolate> {
        Isolate::new()
    }

    fn context(isolate: &mut Isolate) -> Context {
        Context::new(isolate).unwrap()
    }

    thread_local! {
        static MARKED: Cell<bool> = const { Cell::new(false) };
    }

    /// Install `stop` and `mark` on the realm's global: `stop` asks the running
    /// execution to stop (what `deno_core`'s `reportUnhandledException` ends in),
    /// `mark` records that it ran. Both are returned as values, so a test can also
    /// hand them to a JS function as arguments — which is the shape that lowers to
    /// the fast slot call a host's `Deno.core.ops` call takes.
    fn install_termination_pair(isolate: &mut Isolate) -> (Value, Value) {
        fn stop(_this: &Value, _args: &[Value]) -> Result<Value, JsError> {
            let agent = crux::function::current_agent();
            assert!(!agent.is_null(), "a builtin runs inside an agent window");
            // SAFETY: the engine recorded this agent for the duration of the call.
            unsafe { &*(agent as *const crate::agent::Agent) }.request_termination();
            Ok(Value::Undefined)
        }
        fn mark(_this: &Value, _args: &[Value]) -> Result<Value, JsError> {
            MARKED.with(|marked| marked.set(true));
            Ok(Value::Undefined)
        }

        MARKED.with(|marked| marked.set(false));
        let realm = isolate.agent.current_realm().expect("a realm");
        let mut values = Vec::new();
        for (name, call) in [
            (
                "stop",
                stop as fn(&Value, &[Value]) -> Result<Value, JsError>,
            ),
            ("mark", mark),
        ] {
            let function = crux::function::Function::create_builtin(
                Some(crux::string::JsString::from_utf8(name)),
                0,
                Box::new(call),
                None,
                None,
            )
            .expect("a builtin");
            let value = Value::Function(function);
            realm
                .global_object
                .define_property_or_throw(
                    &crux::string::JsString::from_utf8(name),
                    &crux::property::PropertyDescriptor {
                        value: Some(value),
                        writable: Some(true),
                        get: None,
                        set: None,
                        enumerable: Some(false),
                        configurable: Some(true),
                    },
                )
                .expect("defined");
            values.push(value);
        }
        (values[0], values[1])
    }

    /// A request stops the next call — here a plain call to a host callback,
    /// which is the shape a host's own op call takes.
    #[test]
    fn a_termination_request_stops_the_next_call() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        install_termination_pair(&mut isolate);

        let _ = context.eval(
            "try { stop(); } catch (error) {}\n\
             try { mark(); } catch (error) {}",
        );
        assert!(
            !MARKED.with(|marked| marked.get()),
            "the call after the request is refused"
        );
    }

    /// The shared fast core is its own check point: a certified leaf is run
    /// inline on the spot, so a call to one never reaches `call_inner` — the
    /// check at the end of the core is too late for it. A cold leaf cache is
    /// what puts the request in front of the core rather than in front of a
    /// cached entry (the warm case is its own test below).
    #[test]
    fn a_termination_request_stops_a_leaf_call_in_the_fast_core() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        install_termination_pair(&mut isolate);

        let _ = context.eval(
            "function leafOf(n) { return n; }\n\
             globalThis.ranLeaf = 0;\n\
             stop();\n\
             globalThis.ranLeaf = leafOf(1);",
        );
        isolate.cancel_terminate_execution();
        assert_eq!(
            context
                .eval("globalThis.ranLeaf")
                .to_local_checked()
                .as_number(),
            Some(0.0),
            "the leaf call after the request is refused"
        );
    }

    /// A **warm** leaf cache is a check point of its own: the hit is taken
    /// before the shared core is entered. The earlier call warms the site's
    /// entry, and the global object is left alone afterwards — any write to it
    /// would move its generation and send the call down the cold path instead.
    #[test]
    fn a_termination_request_stops_a_global_call_a_warm_leaf_cache_would_inline() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        install_termination_pair(&mut isolate);

        let _ = context.eval(
            "function leafOf(n) { return n; }\n\
             globalThis.ranCache = 0;\n\
             leafOf(7);\n\
             stop();\n\
             globalThis.ranCache = leafOf(1);",
        );
        isolate.cancel_terminate_execution();
        assert_eq!(
            context
                .eval("globalThis.ranCache")
                .to_local_checked()
                .as_number(),
            Some(0.0),
            "the cached leaf call after the request is refused"
        );
    }

    /// The same for a **slot** callee: `outer`'s parameter resolved to a
    /// certified leaf on the earlier call, so the cached slot entry is a second
    /// call entry that the shared core never sees. The request is raised inside
    /// `outer`, which is what makes the slot call — not the call to `outer` —
    /// the next call.
    #[test]
    fn a_termination_request_stops_a_slot_call_a_warm_leaf_cache_would_inline() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        install_termination_pair(&mut isolate);

        let _ = context.eval(
            "function leafOf(n) { return n; }\n\
             function outer(f, go) { if (go) stop(); return f(1); }\n\
             outer(leafOf, false);\n\
             globalThis.ranSlot = 0;\n\
             globalThis.ranSlot = outer(leafOf, true);",
        );
        isolate.cancel_terminate_execution();
        assert_eq!(
            context
                .eval("globalThis.ranSlot")
                .to_local_checked()
                .as_number(),
            Some(0.0),
            "the cached slot call after the request is refused"
        );
    }

    /// A tail call is a call: its dispatch inlines a certified leaf or replaces
    /// the frame for anything else, and both start new execution. The body is
    /// strict because a proper tail call is compiled only there.
    #[test]
    fn a_termination_request_stops_a_tail_call() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        install_termination_pair(&mut isolate);

        let _ = context.eval(
            "function leafOf(n) { return n; }\n\
             function caller() { 'use strict'; stop(); return leafOf(1); }\n\
             globalThis.ranTail = 0;\n\
             globalThis.ranTail = caller();",
        );
        isolate.cancel_terminate_execution();
        assert_eq!(
            context
                .eval("globalThis.ranTail")
                .to_local_checked()
                .as_number(),
            Some(0.0),
            "the tail call after the request is refused"
        );
    }

    #[test]
    fn eval_returns_the_completion_value() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        assert_eq!(
            context.eval("1 + 2").to_local_checked().as_number(),
            Some(3.0)
        );
    }

    /// The text a call frame runs is the **callee's**, not the caller's: a body
    /// whose span belongs to one script still hands its closures that text when
    /// it is called from a *second*, differently-sized one.
    ///
    /// The caller's text is right only when the two happen to be the same one —
    /// which every single-source program hides — and the difference is not
    /// cosmetic: a closure's `Function.prototype.toString` is its span's slice,
    /// and a span resolved against the wrong text is either a wrong slice or, as
    /// here, past the end of it.
    #[test]
    fn a_called_body_runs_its_own_text_not_the_callers() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        // One shape per call path: a certified body (frame slots), an arrow
        // (whose record carries the enclosing text of its own creation site),
        // a body the scope analysis cannot certify (a default parameter),
        // which takes the environment path, and a tail call, which replaces the
        // running frame rather than pushing one.
        context
            .try_eval(
                "globalThis.makeCertified = function () { return function stub() { return 41; }; };",
            )
            .expect("the certified factory's script");
        context
            .try_eval("globalThis.makeArrow = () => function stub() { return 41; };")
            .expect("the arrow factory's script");
        context
            .try_eval(
                "globalThis.makeEnv = function (a = 1) { return function stub() { return 41; }; };",
            )
            .expect("the env factory's script");
        // A `Function`-built body: its frame carries the assembled string its
        // spans belong to, which is the only text it has.
        context
            .try_eval(
                "globalThis.makeDynamic = new Function('return function stub() { return 41; };');",
            )
            .expect("the dynamic factory's script");
        // The two tail paths: a strict body whose last act is a call replaces
        // the caller's frame instead of pushing onto it, and the callee may be
        // either certified or an env-path body.
        context
            .try_eval(
                "globalThis.makeTail = function () { 'use strict'; return tailCertified(); }; \
                 globalThis.tailCertified = function () { return function stub() { return 41; }; }; \
                 globalThis.makeTailEnv = function () { 'use strict'; return tailEnv(); }; \
                 globalThis.tailEnv = function (a = 1) { return function stub() { return 41; }; };",
            )
            .expect("the tail factories' script");
        for call in [
            "makeCertified()",
            "makeArrow()",
            "makeEnv()",
            "makeDynamic()",
            "makeTail()",
            "makeTailEnv()",
        ] {
            let made = context.try_eval(call).expect("the call");
            assert_eq!(
                isolate.function_source(&made).as_deref(),
                Some("function stub() { return 41; }"),
                "the span belongs to the factory's script, not the caller's: {call}"
            );
        }
    }

    /// The suspended bodies resume under the context their call pushed, so the
    /// text an async function, a generator or an async generator captures at
    /// call time is the text their bodies run under.
    ///
    /// Each is driven from a second script, which is what makes these three
    /// paths distinguishable from the ordinary call above.
    #[test]
    fn a_suspended_bodys_frame_carries_its_own_text() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let cases: [(&str, &str); 3] = [
            (
                "globalThis.make = async function () { return function stub() { return 41; }; };",
                "make().then(v => { globalThis.made = v; });",
            ),
            (
                "globalThis.make = function* () { return function stub() { return 41; }; };",
                "globalThis.made = make().next().value;",
            ),
            (
                "globalThis.make = async function* () { return function stub() { return 41; }; };",
                "make().next().then(r => { globalThis.made = r.value; });",
            ),
        ];
        for (define, drive) in cases {
            context.try_eval(define).expect("the definition");
            context.try_eval(drive).expect("the drive");
            context.run_microtasks().expect("the microtasks");
            let made = context.try_eval("globalThis.made").expect("the closure");
            assert_eq!(
                isolate.function_source(&made).as_deref(),
                Some("function stub() { return 41; }"),
                "a resumed body's frame carries its own text: {define}"
            );
        }
    }

    /// A module function registered at link time carries its **module's** text,
    /// so a closure it creates resolves its span even when the host calls it
    /// directly — with no module frame beneath the call to widen to.
    ///
    /// Link runs under a context with no text of its own, so the module's text is
    /// the one the declaration pass has to record; what the walk finds instead is
    /// the function's own slice, which a nested closure's span runs past when the
    /// function does not happen to start at the module's first offset.
    #[test]
    fn a_module_functions_frame_carries_the_module_text() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let module = Module::compile_with_name(
            &context,
            "entry",
            Some("file:///app/main.js"),
            "const pad = 0;\n\
             function make() { return function stub() { return 41; }; }\n\
             globalThis.make = make;\n\
             export { make };",
        )
        .expect("compile");
        module.instantiate(&context).expect("instantiate");
        module.evaluate(&context).expect("evaluate");
        let make = context.try_eval("globalThis.make").expect("the export");
        let made = context
            .try_call(&make, &Local::undefined(), &[])
            .expect("the host's call");
        assert_eq!(
            isolate.function_source(&made).as_deref(),
            Some("function stub() { return 41; }"),
            "the closure's span belongs to the module's text"
        );
    }

    /// A certified constructor's frame carries its own text too — the construct
    /// path is a second push site with the same obligation.
    #[test]
    fn a_constructed_bodys_frame_carries_its_own_text() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        context
            .try_eval("globalThis.C = function () { this.stub = function stub() { return 41; }; };")
            .expect("the constructor's script");
        context
            .try_eval("globalThis.instance = new C();")
            .expect("the construct");
        let made = context.try_eval("instance.stub").expect("the closure");
        assert_eq!(
            isolate.function_source(&made).as_deref(),
            Some("function stub() { return 41; }"),
            "the constructed body's frame carries its own text"
        );
    }

    #[test]
    fn eval_failure_sets_the_pending_exception() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let result = context.eval("throw new TypeError('boom')");
        assert!(result.is_empty());
        assert!(isolate.has_pending_exception());
        let exception = isolate.pending_exception().unwrap();
        assert_eq!(crux::value::type_of(&exception), "object");
        // The native error constructor was used, so the thrown value has a
        // `name` of TypeError.
        let exception_object = crate::context::as_object(&exception).unwrap();
        let name = exception_object
            .get(&crux::string::JsString::from_utf8("name"))
            .unwrap();
        assert_eq!(name.to_string(), "TypeError");
    }

    #[test]
    fn a_termination_request_stops_a_running_loop() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        // The request comes from another thread, which is the shape a host has:
        // the isolate cannot leave its own thread, and a watchdog can.
        let flag = isolate.termination_flag();
        let watchdog = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        // The loop is huge but finite, so a check point that did not observe the
        // request runs it to completion and fails the assertion rather than
        // hanging the suite — and the request lands at 50ms, long before.
        assert!(
            context.eval("for (var i = 0; i < 1e7; i++) {}").is_empty(),
            "the loop stopped"
        );
        let exception = isolate.pending_exception().expect("the termination error");
        let exception = crate::context::as_object(&exception).unwrap();
        let name = exception
            .get(&crux::string::JsString::from_utf8("name"))
            .unwrap();
        let message = exception
            .get(&crux::string::JsString::from_utf8("message"))
            .unwrap();
        assert_eq!(name.to_string(), "Error");
        assert_eq!(message.to_string(), "execution terminated");

        // The request stands until it is withdrawn, and then the isolate runs
        // again.
        assert!(isolate.is_execution_terminating());
        isolate.cancel_terminate_execution();
        assert!(!context.eval("1 + 1").is_empty());
        watchdog.join().unwrap();
    }

    /// A string-building loop that allocates steadily, bounded so a limit that
    /// never fires fails a test rather than hanging it. An IIFE, because the
    /// tests run it more than once and a script-level `let` cannot be declared
    /// twice in one realm.
    const ALLOCATING_LOOP: &str =
        "(function () { let s = ''; for (let i = 0; i < 20000; i++) s += 'xxxxxxxxxxxxxxxx'; })();";

    /// A host's `heap_limits` are enforced against the live heap, and the
    /// callback a heap that reaches them runs is handed the host's own numbers
    /// and answers the limit to continue with — which is how a host that raises
    /// the limit keeps a loop going and how `deno_core` asserts the first of two
    /// callbacks never runs.
    #[test]
    fn a_heap_limit_fires_the_hosts_callback_with_the_hosts_own_limits() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        const MAX: usize = 64 * 1024;
        isolate.set_heap_limits(0, MAX);
        let seen = Rc::new(std::cell::RefCell::new(Vec::<(usize, usize)>::new()));
        let inner = Rc::clone(&seen);
        isolate.set_near_heap_limit_callback(Some(Box::new(move |current, initial| {
            inner.borrow_mut().push((current, initial));
            current * 2
        })));
        assert!(
            !context.eval(ALLOCATING_LOOP).is_empty(),
            "the loop completed"
        );
        let firings = seen.borrow().len();
        assert!(
            firings >= 2,
            "the callback ran at least twice, saw {firings}"
        );
        assert_eq!(
            seen.borrow()[0],
            (MAX, 0),
            "the host's own limits are what it is handed"
        );
        assert_eq!(
            seen.borrow()[1].0,
            MAX * 2,
            "the answer replaces the limit, so the next firing is at the doubled one"
        );
        // Withdrawn, it stops running: the limit is dropped far below anything
        // the loop allocates, so a callback still installed would fire at once
        // and the run staying clean is what says it was withdrawn.
        isolate.remove_near_heap_limit_callback(0);
        assert_eq!(isolate.heap_limit(), Some(MAX * (1 << firings)));
        isolate.set_heap_limits(0, 1024);
        assert!(!context.eval(ALLOCATING_LOOP).is_empty());
        assert_eq!(seen.borrow().len(), firings, "a withdrawn callback is gone");
    }

    /// The shape deno's heap-limit tests use: the callback asks the running
    /// execution to stop, so the loop ends at the next check point with the same
    /// error a JS termination produces.
    #[test]
    fn a_heap_limit_callback_that_terminates_stops_the_run() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        isolate.set_heap_limits(0, 64 * 1024);
        let flag = isolate.termination_flag();
        isolate.set_near_heap_limit_callback(Some(Box::new(move |current, _initial| {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            current * 2
        })));
        assert!(context.eval(ALLOCATING_LOOP).is_empty(), "the loop stopped");
        let exception = isolate.pending_exception().expect("the termination error");
        let exception = crate::context::as_object(&exception).unwrap();
        let name = exception
            .get(&crux::string::JsString::from_utf8("name"))
            .unwrap();
        let message = exception
            .get(&crux::string::JsString::from_utf8("message"))
            .unwrap();
        assert_eq!(name.to_string(), "Error");
        assert_eq!(message.to_string(), "execution terminated");
    }

    #[test]
    fn a_termination_request_is_seen_at_the_next_entry_and_by_a_try_catch() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        isolate.terminate_execution();
        {
            let try_catch = TryCatch::new(&mut isolate);
            // A script with no loop and no call of its own has only the entry
            // check point, and it is refused there.
            assert!(context.eval("1 + 1").is_empty());
            assert!(try_catch.has_caught());
            assert!(try_catch.has_terminated());
        }
        isolate.cancel_terminate_execution();
        assert!(!context.eval("1 + 1").is_empty(), "usable again");
    }

    #[test]
    fn try_catch_observes_and_clears_the_pending_exception() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        {
            let try_catch = TryCatch::new(&mut isolate);
            let result = context.eval("throw new Error('boom')");
            assert!(result.is_empty());
            assert!(try_catch.has_caught());
            assert!(try_catch.exception().unwrap().is_object());
        }
        assert!(!isolate.has_pending_exception());
    }

    /// A handler owns what it caught: asking for the exception moves it out of
    /// the isolate's slot, so a host that has read (and dealt with) an exception
    /// can call back into JS without the engine still holding it. V8's handler
    /// does the same, and deno's error path depends on it: it reads the caught
    /// exception and then calls the realm's format-exception callback.
    #[test]
    fn a_handler_owns_the_exception_it_caught() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        {
            let try_catch = TryCatch::new(&mut isolate);
            let result = context.eval("throw new Error('boom')");
            assert!(result.is_empty());
            assert!(try_catch.has_caught());
            let caught = try_catch.exception().expect("the caught exception");
            assert!(caught.is_object());
            assert!(
                !isolate.has_pending_exception(),
                "reading it handed it to the handler"
            );
            assert!(
                try_catch.exception().is_some(),
                "and the handler still answers it"
            );
            try_catch.reset();
            assert!(!try_catch.has_caught(), "a reset drops it");
        }
        assert!(!isolate.has_pending_exception());
    }

    #[test]
    fn rethrow_keeps_the_exception_pending() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        {
            let try_catch = TryCatch::new(&mut isolate);
            let _ = context.eval("throw new Error('boom')");
            assert!(try_catch.has_caught());
            try_catch.rethrow();
        }
        assert!(isolate.has_pending_exception());
    }

    #[test]
    fn exception_throws_native_errors() {
        let mut isolate = isolate();
        let _context = context(&mut isolate);
        let error = Exception::throw_type_error(&mut isolate, "nope").unwrap();
        assert!(error.is_object());
        assert!(isolate.has_pending_exception());
    }

    #[test]
    fn creating_an_error_leaves_the_pending_exception_alone() {
        let mut isolate = isolate();
        let _context = context(&mut isolate);
        let error = Exception::create_with(&mut isolate, "%TypeError%", "nope").unwrap();
        // A create is not a throw (V8's `Exception::TypeError` does not set a
        // pending exception; `Isolate::ThrowException` does).
        assert!(!isolate.has_pending_exception());
        assert!(error.is_object());
        let object = crate::context::as_object(error.value()).unwrap();
        let name = object
            .get(&crux::string::JsString::from_utf8("name"))
            .unwrap();
        let message = object
            .get(&crux::string::JsString::from_utf8("message"))
            .unwrap();
        assert_eq!(name.to_string(), "TypeError");
        assert_eq!(message.to_string(), "nope");
    }

    #[test]
    fn host_function_is_callable_from_js() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let template = FunctionTemplate::new(
            &mut isolate,
            Box::new(|info| {
                let sum: f64 = info.args().filter_map(|arg| arg.as_number()).sum();
                info.get_return_value().set_number(sum);
            }),
        );
        template.set_class_name("sum");
        let function = template.get_function(&context).unwrap();
        Object::set(&context, &context.global(), "sum", &function, true).unwrap();
        assert_eq!(
            context.eval("sum(1, 2, 3)").to_local_checked().as_number(),
            Some(6.0)
        );
        assert_eq!(
            context
                .eval("sum.name")
                .to_local_checked()
                .as_string()
                .as_deref(),
            Some("sum")
        );
    }

    /// The number a global holds, or `None` when it is not one.
    fn read_number(context: &Context, name: &str) -> Option<f64> {
        Object::get(context, &context.global(), name)
            .ok()
            .and_then(|value| value.as_number())
    }

    /// `Explicit` leaves the queues to the host, which is the engine's default.
    #[test]
    fn explicit_leaves_the_queues_to_the_host() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        context
            .try_eval("Promise.resolve().then(() => { globalThis.ran = 1; })")
            .expect("eval");
        assert_eq!(read_number(&context, "ran"), None);
        context.run_microtasks().expect("drain");
        assert_eq!(read_number(&context, "ran"), Some(1.0));
    }

    /// `Auto` drains when the *outermost* entry returns: a callback that calls
    /// back into the engine raises the depth, so no job runs under a callback
    /// that is still on the stack.
    #[test]
    fn auto_drains_at_the_outer_entry_only() {
        let mut isolate = isolate();
        isolate.set_microtasks_policy(MicrotasksPolicy::Auto);
        let context = context(&mut isolate);

        let still_queued = Rc::new(Cell::new(false));
        let observed = still_queued.clone();
        let nested = context;
        let template = FunctionTemplate::new(
            &mut isolate,
            Box::new(move |info| {
                nested
                    .try_eval("Promise.resolve().then(() => { globalThis.ran = 1; })")
                    .expect("nested eval");
                // SAFETY: a call is on the stack, so the isolate is alive.
                let agent = unsafe { &*nested.isolate() };
                observed.set(!unsafe { &*agent.agent_ptr() }.job_queues_empty());
                info.get_return_value().set_undefined();
            }),
        );
        let function = template.get_function(&context).expect("function");
        Object::set(&context, &context.global(), "probe", &function, true).expect("set");

        context.eval("probe()").to_local_checked();
        assert!(
            still_queued.get(),
            "the callback's own entry drained the queues under it"
        );
        assert_eq!(read_number(&context, "ran"), Some(1.0));
    }

    #[test]
    fn host_callback_receives_this_and_args() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let observed_this: Rc<RefCell<Option<Value>>> = Rc::new(RefCell::new(None));
        let observed = observed_this.clone();
        let template = FunctionTemplate::new(
            &mut isolate,
            Box::new(move |info| {
                *observed.borrow_mut() = Some(info.this().into_value());
                info.get_return_value()
                    .set(info.arg(0).unwrap_or(Local::undefined()));
            }),
        );
        let function = template.get_function(&context).unwrap();
        Object::set(&context, &context.global(), "echo", &function, true).unwrap();
        let result = context
            .try_eval("globalThis.hold = { y: 'obj' }; ({ x: 'hi' }).x = echo.call(globalThis.hold, 'arg')")
            .unwrap_or_else(|error| panic!("eval failed: {error}"));
        assert_eq!(result.as_string().as_deref(), Some("arg"));
        // `this` inside the callback is the `call` receiver, an object. The
        // callback stashed the value in a native cell, so the receiver must
        // stay reachable from a traced root (`globalThis.hold`) until the
        // read-back — a Value held only in native memory would be swept
        // (GC model; the pre-GC Rc handle kept it alive implicitly).
        let this = (*observed_this.borrow()).unwrap();
        assert!(this.is_object());
        let this_object = crate::context::as_object(&this).unwrap();
        let y = this_object
            .get(&crux::string::JsString::from_utf8("y"))
            .unwrap();
        assert_eq!(y.to_string(), "obj");
    }

    #[test]
    fn host_callback_throw_propagates_to_js() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let template = FunctionTemplate::new(
            &mut isolate,
            Box::new(|info| {
                unsafe { &*info.isolate() }
                    .throw_exception(Local::string("host threw").into_value());
            }),
        );
        let function = template.get_function(&context).unwrap();
        Object::set(&context, &context.global(), "boom", &function, true).unwrap();
        let result = context
            .eval("try { boom(); 'not reached' } catch (e) { e }")
            .to_local_checked();
        assert_eq!(result.as_string().as_deref(), Some("host threw"));
        assert!(!isolate.has_pending_exception());
    }

    #[test]
    fn host_constructor_creates_instances() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let template = FunctionTemplate::new(
            &mut isolate,
            Box::new(|info| {
                if info.is_construct_call() {
                    info.get_return_value().set(info.this());
                } else {
                    info.get_return_value().set_number(0.0);
                }
            }),
        );
        template.set_class_name("Point");
        template.instance_template().set("x", Local::number(1.0));
        let constructor = template.get_function(&context).unwrap();
        Object::set(&context, &context.global(), "Point", &constructor, true).unwrap();

        let instance = context.eval("new Point()").to_local_checked();
        assert_eq!(
            Object::get(&context, &instance, "x").unwrap().as_number(),
            Some(1.0)
        );
        assert_eq!(
            context
                .eval("new Point() instanceof Point")
                .to_local_checked()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            context.eval("Point()").to_local_checked().as_number(),
            Some(0.0)
        );
    }

    #[test]
    fn object_template_accessors_route_to_callbacks() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let template = ObjectTemplate::new(&mut isolate);
        let stored: Rc<Cell<f64>> = Rc::new(Cell::new(7.0));
        let getter_stored = stored.clone();
        let setter_stored = stored.clone();
        template.set_accessor(
            "x",
            Box::new(move |info| {
                info.get_return_value().set_number(getter_stored.get());
            }),
            Some(Box::new(move |info| {
                setter_stored.set(info.arg(0).and_then(|arg| arg.as_number()).unwrap_or(0.0));
                info.get_return_value().set_undefined();
            })),
        );
        let object = template.new_instance(&context).unwrap();
        assert_eq!(
            Object::get(&context, &object, "x").unwrap().as_number(),
            Some(7.0)
        );
        Object::set(&context, &object, "x", &Local::number(9.0), true).unwrap();
        assert_eq!(stored.get(), 9.0);
        assert_eq!(
            Object::get(&context, &object, "x").unwrap().as_number(),
            Some(9.0)
        );
    }

    #[test]
    fn external_round_trips_host_pointers() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let marker: *mut std::ffi::c_void = 0xDEAD as *mut std::ffi::c_void;
        let external = External::new(&mut isolate, marker).unwrap();
        assert_eq!(external.value(), marker);
        Object::set(
            &context,
            &context.global(),
            "ext",
            &external.as_value(),
            true,
        )
        .unwrap();
        let back = context.eval("ext").to_local_checked();
        assert_eq!(
            back.as_object().unwrap().id(),
            external.as_value().as_object().unwrap().id()
        );
    }

    #[test]
    fn object_and_array_helpers() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let object = Object::new(&context).unwrap();
        Object::set(&context, &object, "a", &Local::number(1.0), true).unwrap();
        assert_eq!(
            Object::get(&context, &object, "a").unwrap().as_number(),
            Some(1.0)
        );
        assert!(Object::has(&context, &object, "a").unwrap());
        assert!(Object::delete(&context, &object, "a").unwrap());
        assert!(!Object::has(&context, &object, "a").unwrap());

        let array = Array::new(&context, &[Local::number(1.0), Local::number(2.0)]).unwrap();
        assert_eq!(Array::length(&context, &array).unwrap(), 2.0);
        assert_eq!(
            Array::get(&context, &array, 1).unwrap().as_number(),
            Some(2.0)
        );
        Object::set(&context, &context.global(), "a", &array, true).unwrap();
        assert_eq!(
            context
                .eval("Array.isArray(a)")
                .to_local_checked()
                .as_boolean(),
            Some(true)
        );
    }

    #[test]
    fn json_round_trip() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let parsed = Json::parse(&context, r#"{"a": 1}"#).unwrap();
        assert_eq!(
            Object::get(&context, &parsed, "a").unwrap().as_number(),
            Some(1.0)
        );
        let text = Json::stringify(&context, &parsed).unwrap();
        assert_eq!(text.as_string().as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn promise_helpers_read_state_and_run_microtasks() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let promise = Promise::resolve(&context, &Local::number(42.0)).unwrap();
        assert_eq!(Promise::state(&context, &promise).unwrap(), "fulfilled");
        assert_eq!(
            Promise::result(&context, &promise).unwrap().as_number(),
            Some(42.0)
        );

        let chained = Promise::then(&context, &promise, None, None).unwrap();
        assert_eq!(Promise::state(&context, &chained).unwrap(), "pending");
        context.run_microtasks().unwrap();
        assert_eq!(
            Promise::result(&context, &chained).unwrap().as_number(),
            Some(42.0)
        );
    }

    #[test]
    fn script_compile_surfaces_syntax_errors() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        assert!(Script::compile(&context, "function (").is_err());
        let script = Script::compile(&context, "1 + 2").unwrap();
        assert_eq!(script.try_run(&context).unwrap().as_number(), Some(3.0));
    }

    #[test]
    fn handles_mirror_v8_shapes() {
        let local = Local::number(1.0);
        let global = Global::new(local);
        assert!(!global.is_empty());
        assert_eq!(global.get().as_number(), Some(1.0));
        let mut empty = Global::empty();
        assert!(empty.is_empty());
        empty.reset(Local::string("hi"));
        assert_eq!(empty.get().as_string().as_deref(), Some("hi"));
        empty.clear();
        assert!(empty.is_empty());

        let _scope = HandleScope::new();
        let maybe = MaybeLocal::Some(Local::number(1.0));
        assert!(!maybe.is_empty());
        assert_eq!(maybe.to_local().unwrap().as_number(), Some(1.0));
        assert!(MaybeLocal::Nothing.to_local().is_none());
    }

    #[test]
    fn isolate_data_slots() {
        let isolate = isolate();
        assert_eq!(isolate.get_data(1), None);
        isolate.set_data(1, 42);
        assert_eq!(isolate.get_data(1), Some(42));
    }
}
