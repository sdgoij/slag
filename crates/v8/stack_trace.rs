//! The running call stack (`v8::StackTrace`, `v8::StackFrame`).
//!
//! A capture is a snapshot of the activations that are on the stack when a host
//! asks for one — the engine's **execution contexts**, which is one per call
//! except for a *leaf-inlined* one: a leaf body is run in place on its caller's
//! Vm by design (a leaf creates no closures and reads no spec-only slot, both of
//! which disqualify leafhood), so it pushes no context of its own. That is the
//! only activation a trace can omit, and JavaScript cannot observe the omission,
//! because reaching a trace at all means making a call. That is measured, not
//! assumed — the test below pins it — and `runtime::api::stack_trace`'s module
//! documentation is where it is spelled out. What each frame knows, and what it
//! deliberately does not, is the engine's story too; `.notes/embedding.md` §9
//! states the divergences (no line or column, no function name for a call that
//! does not read `arguments`, `false` for eval/constructor/wasm, `true` for user
//! JavaScript).
//!
//! Two things are the bridge's own. A capture names an ordinary object it mints,
//! because a handle has to name something the collector can see: a host that
//! keeps the trace (in a `Global`, say) keeps its frames, and a host that drops
//! it lets the collector prune them. And a frame handle is a position in a
//! capture rather than a value of its own — the shape `v8::ModuleRequest` has
//! here — so reading a frame whose capture is gone answers nothing rather than a
//! stale frame.

use runtime::api;

use crate::data::{StackFrame, StackTrace};
use crate::handle::{Local, LocalHandle};
use crate::scope::PinScope;

/// The isolate whose stack a capture would be taken from.
///
/// These accessors take no scope — the crate we stand in for's take none either
/// — so the isolate is the one whose realm this thread has entered; with none
/// entered there is no stack to report.
fn isolate() -> Option<crate::Isolate> {
    Some(crate::scope::isolate_of(crate::realm::current()?))
}

impl StackTrace {
    /// Capture the running stack
    /// (v8::StackTrace::CurrentStackTrace): at most `frame_limit` frames,
    /// innermost first.
    ///
    /// `None` for a limit of zero — V8's own refusal, and its null handle — and
    /// for a capture outside a realm, where there is no stack to report.
    pub fn current_stack_trace<'a>(
        scope: &PinScope<'a, '_>,
        frame_limit: usize,
    ) -> Option<Local<'a, StackTrace>> {
        let mut isolate = crate::scope::isolate_of(crate::realm_of(scope));
        isolate
            .engine_mut()
            .capture_stack(frame_limit)
            .map(Local::from_engine)
    }
}

impl<'s> LocalHandle<'s, StackTrace> {
    /// The number of frames a capture holds
    /// (v8::StackTrace::GetFrameCount).
    ///
    /// Zero for a capture whose object the collector has reaped, which is the
    /// same statement as "the host stopped holding it".
    pub fn get_frame_count(&self) -> usize {
        isolate().map_or(0, |mut isolate| {
            isolate.engine_mut().captured_frame_count(self.engine())
        })
    }

    /// One frame of a capture (v8::StackTrace::GetFrame), or `None` for an index
    /// past its end.
    pub fn get_frame<'a>(
        &self,
        _scope: &PinScope<'a, '_>,
        index: usize,
    ) -> Option<Local<'a, StackFrame>> {
        if index >= self.get_frame_count() {
            return None;
        }
        Some(Local::from_payload(crate::handle::Payload::StackFrame {
            trace: *self.engine(),
            index: index as u32,
        }))
    }
}

impl<'s> LocalHandle<'s, StackFrame> {
    /// The frame itself, from the capture it belongs to.
    ///
    /// Every accessor below reads one of its fields, so a frame whose capture's
    /// object the collector has reaped answers the "no information" value — the
    /// same statement as "the host stopped holding the capture".
    fn frame(&self) -> Option<api::StackFrame> {
        let (trace, index) = self.payload().as_stack_frame()?;
        isolate()
            .and_then(|mut isolate| isolate.engine_mut().captured_frame(&trace, index as usize))
    }

    /// The number, 1-based, of the line the frame is on
    /// (v8::StackFrame::GetLineNumber).
    ///
    /// `Message::kNoLineNumberInfo` — 0 — always: the engine records no position
    /// per activation. See the module documentation.
    pub fn get_line_number(&self) -> usize {
        self.frame().map_or(0, |frame| frame.line)
    }

    /// The 1-based column the frame is on (v8::StackFrame::GetColumn).
    ///
    /// `Message::kNoColumnInfo` — 0 — always, for the reason above.
    pub fn get_column(&self) -> usize {
        self.frame().map_or(0, |frame| frame.column)
    }

    /// The id of the script the frame runs (v8::StackFrame::GetScriptId).
    ///
    /// `Message::kNoScriptIdInfo` — 0 — always: a script here is its source text
    /// and has no id.
    pub fn get_script_id(&self) -> usize {
        0
    }

    /// The name of the code the frame is running
    /// (v8::StackFrame::GetScriptName), or `None` when nothing named it.
    ///
    /// A module's is the name its host compiled it under; a classic script's is
    /// absent, because the engine parses one from text alone — see the module
    /// documentation.
    pub fn get_script_name<'a>(
        &self,
        _scope: &PinScope<'a, '_>,
    ) -> Option<Local<'a, crate::data::String>> {
        self.frame()?
            .script_name
            .map(|name| Local::from_engine(api::Local::string(name)))
    }

    /// The same name, or the source URL the source declares when no name was
    /// recorded (v8::StackFrame::GetScriptNameOrSourceURL).
    ///
    /// The same answer as [`get_script_name`](Self::get_script_name): a `//#
    /// sourceURL=` comment is a script's *fallback* name, and this bridge has no
    /// fallback to offer — the name a host gave is the name.
    pub fn get_script_name_or_source_url<'a>(
        &self,
        scope: &PinScope<'a, '_>,
    ) -> Option<Local<'a, crate::data::String>> {
        self.get_script_name(scope)
    }

    /// The frame's function name (v8::StackFrame::GetFunctionName), or `None`
    /// for a frame with no named function — a script's top level, an anonymous
    /// function, or a *call* the engine pushed a context for without recording
    /// the callee in it: the frame's `function` slot is filled only for the one
    /// reader certification leaves in it, a sloppy body's mapped `arguments`.
    pub fn get_function_name<'a>(
        &self,
        _scope: &PinScope<'a, '_>,
    ) -> Option<Local<'a, crate::data::String>> {
        self.frame()?
            .function_name
            .map(|name| Local::from_engine(api::Local::string(name)))
    }

    /// Whether the frame's function was compiled from `eval` code
    /// (v8::StackFrame::IsEval).
    ///
    /// `false` always: an execution context does not record it, and eval code
    /// inherits its caller's script or module, so nothing else tells them apart.
    pub fn is_eval(&self) -> bool {
        false
    }

    /// Whether the frame's function was called as a constructor
    /// (v8::StackFrame::IsConstructor).
    ///
    /// `false` always: the activation's `new.target` is not kept where a frame
    /// could read it.
    pub fn is_constructor(&self) -> bool {
        false
    }

    /// Whether the frame is in wasm code (v8::StackFrame::IsWasm).
    ///
    /// `false` always: wasm runs through this same context stack with no marker
    /// of its own.
    pub fn is_wasm(&self) -> bool {
        false
    }

    /// Whether the frame is the embedder's own JavaScript
    /// (v8::StackFrame::IsUserJavaScript).
    ///
    /// `true` for every frame: the engine's Rust builtins never push an
    /// execution context, and it classifies no script as native, so every frame
    /// a host can see is JavaScript the engine is running.
    pub fn is_user_javascript(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{FixedArray, Function, Module, String as JsString, Value};
    use crate::handle::Local as BridgeLocal;
    use crate::test_support::{bind, eval, in_context};
    use crate::{Context, FunctionBuilder, FunctionCallbackArguments, ReturnValue};

    /// The crate's `escapable_handle_scope!` in the shape a host uses it — the
    /// shape `deno_core`'s own `JsRuntime::eval` has: an escapable scope opened
    /// over the scope a function was handed, with the script's value escaping to
    /// the caller's lifetime.
    ///
    /// This macro had never been exercised anywhere in this workspace, and the
    /// shape does not type-check on its own; the test is what pins the fix.
    fn eval_in<'s, 'i, T>(scope: &mut PinScope<'s, 'i>, code: &str) -> Option<Local<'s, T>>
    where
        Local<'s, T>: TryFrom<Local<'s, Value>, Error = crate::DataError>,
    {
        crate::escapable_handle_scope!(let scope, scope);
        let source = JsString::new(scope, code).expect("string");
        let script = crate::Script::compile(scope, source, None).expect("compile");
        let value = script.run(scope)?;
        scope.escape(value).try_into().ok()
    }

    #[test]
    fn an_escapable_scope_escapes_a_scripts_value_to_the_callers_lifetime() {
        in_context!(scope, {
            let value = eval_in::<crate::data::Number>(scope, "40 + 2").expect("a value");
            assert_eq!(value.value(), 42.0);
        });
    }

    /// A host function that captures the stack where it is called and records
    /// what each frame said, so a test can assert on a capture taken inside a
    /// running stack.
    fn record_stack(
        scope: &mut PinScope<'_, '_>,
        _args: FunctionCallbackArguments,
        rv: ReturnValue,
    ) {
        let trace = StackTrace::current_stack_trace(scope, 10).expect("a capture");
        let count = trace.get_frame_count();
        let frames: Vec<String> = (0..count)
            .map(|index| {
                let frame = trace.get_frame(scope, index).expect("a frame");
                let function = frame
                    .get_function_name(scope)
                    .map(|name| name.to_rust_string_lossy(scope))
                    .unwrap_or_else(|| "-".into());
                let script = frame
                    .get_script_name(scope)
                    .map(|name| name.to_rust_string_lossy(scope))
                    .unwrap_or_else(|| "-".into());
                assert!(!frame.is_eval() && !frame.is_constructor() && !frame.is_wasm());
                assert!(frame.is_user_javascript());
                assert_eq!(frame.get_script_id(), 0);
                // The site is part of the record now: `line:column`, 1-based,
                // which is what a host reads a frame's position from.
                format!(
                    "{function}@{script}:{}:{}",
                    frame.get_line_number(),
                    frame.get_column()
                )
            })
            .collect();
        FRAMES.with(|seen| *seen.borrow_mut() = frames);
        rv.set_int32(count as i32);
    }

    thread_local! {
        static FRAMES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// A capture reports the activations the engine tracks as **execution
    /// contexts**, which is one per call — this test pins what that means frame
    /// by frame:
    ///
    /// - a certified call is a frame (the engine pushes a context for it), and
    ///   it is named: the push fills the frame's `function` slot for every call,
    ///   because the reader that wants it is a host's error stack (§7's frames
    ///   record), not only a sloppy body's mapped `arguments`;
    /// - a call whose body reads `arguments` names its frame the same way;
    /// - the script the code came from is always a frame;
    /// - every frame is *at* a site — the call it is about to make — so the frame
    ///   carries a line and a column rather than V8's own `kNoLineNumberInfo`.
    ///
    /// See the module documentation and `.notes/embedding.md` §7/§9.
    #[test]
    fn a_capture_reports_the_engines_contexts_not_the_call_stack() {
        let frames = capture_frames("function inner() { return capture(); } inner();");
        assert_eq!(
            frames.len(),
            2,
            "a certified call and the script: {frames:?}"
        );
        assert_frame(&frames[0], "inner", 1);
        assert_frame(&frames[1], "-", 1);
        let frames = capture_frames(
            "function inner(a) { if (a) { return arguments; } return capture(); } inner(0);",
        );
        assert_eq!(
            frames.len(),
            2,
            "a certified call and the script: {frames:?}"
        );
        assert_frame(&frames[0], "inner", 1);
        assert_frame(&frames[1], "-", 1);
    }

    /// One recorded frame: the function that named it, the line it is at, and the
    /// name of the code it runs — which is unnamed here, because the source this
    /// file compiles carries no origin name. The column is asserted to be present
    /// rather than counted: what it must *be* is the site of the call the frame is
    /// at, and the source that states it sits in the test above.
    fn assert_frame(frame: &str, function: &str, line: usize) {
        let (name, rest) = frame.split_once('@').expect("function@script");
        let mut parts = rest.split(':');
        let script = parts.next().expect("a script name");
        let frame_line: usize = parts.next().expect("a line").parse().expect("a line");
        let column: usize = parts.next().expect("a column").parse().expect("a column");
        assert_eq!(name, function, "the frame's function name: {frame}");
        assert_eq!(script, "-", "the code's name: {frame}");
        assert_eq!(frame_line, line, "the line the frame is at: {frame}");
        assert!(column > 0, "and the column of the site it is at: {frame}");
    }

    /// The frames a source produces when it calls `capture()`.
    fn capture_frames(source: &str) -> Vec<String> {
        FRAMES.with(|seen| seen.borrow_mut().clear());
        in_context!(scope, {
            let capture = FunctionBuilder::<Function>::new(record_stack)
                .build(scope)
                .expect("function");
            bind(scope, "capture", capture.cast::<Value>());
            eval(scope, source);
        });
        FRAMES.with(|seen| seen.borrow().clone())
    }

    /// A module's frames carry the name the host compiled the module under —
    /// the half of `get_script_name` the engine can answer, and what
    /// `deno_core` reads to tell one piece of code from another.
    #[test]
    fn a_modules_frames_carry_the_name_it_was_compiled_under() {
        fn nothing_to_resolve<'s>(
            _context: BridgeLocal<'s, Context>,
            _specifier: BridgeLocal<'s, JsString>,
            _attributes: BridgeLocal<'s, FixedArray>,
            _referrer: BridgeLocal<'s, Module>,
        ) -> Option<BridgeLocal<'s, Module>> {
            None
        }

        in_context!(scope, {
            let capture = FunctionBuilder::<Function>::new(record_stack)
                .build(scope)
                .expect("function");
            bind(scope, "capture", capture.cast::<Value>());

            let realm = crate::realm_of(scope);
            let module = api::Module::compile_with_name(
                &realm,
                "entry",
                Some("file:///app/main.js"),
                "capture();",
            )
            .expect("compile");
            let module = BridgeLocal::<Module>::from_module(module);
            assert_eq!(
                module.instantiate_module(scope, nothing_to_resolve),
                Some(true)
            );
            module.evaluate(scope).expect("evaluate");
        });

        FRAMES.with(|seen| {
            let seen = seen.borrow();
            assert_eq!(
                seen.len(),
                1,
                "the module's top level is the frame: {seen:?}"
            );
            assert!(
                seen[0].starts_with("-@file:///app/main.js:"),
                "named by its host, and at the site it runs: {seen:?}"
            );
        });
    }

    /// A capture where no frame is running holds none, and a limit of zero is
    /// the crate's own refusal rather than an empty capture.
    #[test]
    fn a_capture_of_no_frames_is_empty_and_a_zero_limit_is_refused() {
        in_context!(scope, {
            let trace = StackTrace::current_stack_trace(scope, 10).expect("a capture");
            assert_eq!(trace.get_frame_count(), 0);
            assert!(trace.get_frame(scope, 1).is_none());
            assert!(
                StackTrace::current_stack_trace(scope, 0).is_none(),
                "a limit of zero is V8's own refusal"
            );
        });
    }
}
