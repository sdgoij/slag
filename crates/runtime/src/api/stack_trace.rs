//! The running JavaScript call stack (`v8::StackTrace`, `v8::StackFrame`).
//!
//! A capture is a snapshot: `v8::StackTrace::CurrentStackTrace` reads the
//! activations that are on the stack *now*, and a host that reads the frames
//! after the stack has moved on still sees the frames it captured. The engine
//! has that snapshot already — `Agent::execution_context_stack` — and this
//! module is the frame view of it.
//!
//! # What a frame knows
//!
//! One frame per execution context, innermost first. From the context itself:
//! the function (and so its name, when it has one) and the script or module the
//! code came from (and so the name a host gave that code). The realm's bootstrap
//! context is not a frame — it is the one context with no function, no script or
//! module, and no source, and it is never popped, so without this rule every
//! trace would report a frame for it.
//!
//! # What a frame does not know, and why that is a tier rather than a gap
//!
//! V8's `StackFrame` answers more than the engine records:
//!
//! - **Line and column are 0** — V8's own `Message::kNoLineNumberInfo` and
//!   `kNoColumnInfo`. The engine tracks no source offset per activation: a
//!   position is recorded where an error is *made*, not where a frame is
//!   running, which is the same gap `Message`'s runtime location has
//!   (`.notes/embedding.md` §10 splits that off from this work as the
//!   per-activation source positions).
//! - **`is_eval`, `is_constructor` and `is_wasm` are `false`.** Nothing in an
//!   execution context records them: eval code inherits its caller's script or
//!   module (so it cannot even be told apart by that), the activation's
//!   `new.target` is not kept, and wasm code runs through this same context stack
//!   with no marker of its own.
//! - **`is_user_javascript` is `true` for every frame.** V8 answers `false` for
//!   code that is not the embedder's — a builtin's or an extension's script. The
//!   engine's Rust builtins never push an execution context at all, and it
//!   classifies no script as native, so the honest answer for a frame that
//!   exists is that it is running JavaScript.
//!
//! Each of those is stated rather than implied because a host can read the flag
//! and act on it, and a wrong `false` is worse than a documented one.
//!
//! # What a frame is *not*: the call stack
//!
//! The most important thing to know about this surface: `execution_context_stack`
//! is the spec's **execution context** stack, and the engine does not push one
//! per call. An ordinary call runs the body on the VM's own environment stack;
//! a context is pushed where the spec needs one (a script, a module, eval, an
//! async or generator resumption) and, on the call paths that need it, for a
//! body that reads a spec-only component — the measured case is a sloppy
//! `arguments`, and the engine says so where it makes the choice
//! (`crates/runtime/src/function.rs:2271-2278`).
//!
//! So a frame here is *not* a call, and a trace is coarser than V8's:
//! `function inner() { return capture(); } inner();` reports one frame (the
//! script), while the same call with a body that touches `arguments` reports two
//! (the call, then the script). Measured, not assumed — the bridge's test is what
//! pins it, and `.notes/embedding.md` §7 records the finding and what it means
//! for the plan's own survey.
//!
//! A host reading a trace for "which code am I in" gets a true answer either way,
//! because the script or module a frame belongs to is exactly what it names. A
//! host reading it for "how did I get here" gets the *activations* the engine
//! tracks, which is a subset of the call chain. Closing that gap means the VM's
//! own frames, which is the larger engine item §10 names.
//!
//! # Where a capture lives
//!
//! A capture is held in `Agent::stack_traces` under the **box address of the
//! object the handle names**, and that object is an ordinary one the capture
//! mints purely so the entry has a liveness: the collector's compaction hook
//! drops an entry whose object is dead, so a host that captures traces in a loop
//! does not grow the table (§12's open item about the bridge's position table is
//! the same problem, unsolved there). A host therefore keeps a trace alive the
//! way it keeps any value alive, or holds it in a `Global`.

use crux::value::ValueKind;

use super::Isolate;
use super::context::Context;
use super::handle::Local;
use crate::agent::Agent;
use crate::context::{ExecutionContext, ScriptOrModule};

/// One activation on the running stack (v8::StackFrame).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    /// The frame's function name, when it has one (v8::StackFrame::GetFunctionName).
    pub function_name: Option<String>,
    /// The name of the code the frame is running — the host's `ScriptOrigin`
    /// name for a module, `None` for a classic script
    /// (v8::StackFrame::GetScriptName). See [`Isolate::capture_stack`].
    pub script_name: Option<String>,
    /// The line, 1-based, or 0 for "no line information"
    /// (v8::StackFrame::GetLineNumber).
    pub line: usize,
    /// The column, 1-based, or 0 for "no column information"
    /// (v8::StackFrame::GetColumn).
    pub column: usize,
    pub is_eval: bool,
    pub is_constructor: bool,
    pub is_wasm: bool,
    pub is_user_javascript: bool,
}

impl Isolate {
    /// Capture the running stack (v8::StackTrace::CurrentStackTrace): at most
    /// `limit` frames, innermost first, held under the object this answers.
    ///
    /// `None` only for a limit of zero, which is V8's own refusal (an empty
    /// trace is a trace with no frames, not the null handle). A capture outside
    /// a realm answers nothing either — there is no stack then.
    pub fn capture_stack(&mut self, limit: usize) -> Option<Local> {
        if limit == 0 {
            return None;
        }
        let realm = self.agent.current_realm().ok()?;
        let frames = stack_frames(&self.agent, limit);
        let trace = super::Object::new(&Context::from_realm(self, realm)).ok()?;
        let address = trace.value().as_object()?.as_any().addr();
        self.agent.stack_traces.borrow_mut().insert(address, frames);
        Some(trace)
    }

    /// The number of frames a capture holds
    /// (v8::StackTrace::GetFrameCount).
    ///
    /// Zero for a capture whose object the collector has reaped, which is the
    /// same statement as "the host stopped holding it".
    pub fn captured_frame_count(&self, trace: &Local) -> usize {
        self.captured_frames(trace).map_or(0, |frames| frames.len())
    }

    /// One frame of a capture (v8::StackTrace::GetFrame), or `None` for an index
    /// past the end.
    pub fn captured_frame(&self, trace: &Local, index: usize) -> Option<StackFrame> {
        self.captured_frames(trace)?.get(index).cloned()
    }

    /// The frames behind a capture handle, when the object is still alive.
    fn captured_frames(&self, trace: &Local) -> Option<Vec<StackFrame>> {
        let object = trace.value().as_object()?;
        self.agent
            .stack_traces
            .borrow()
            .get(&object.as_any().addr())
            .cloned()
    }
}

/// The frame view of the agent's execution contexts: innermost first, at most
/// `limit` of them.
pub(crate) fn stack_frames(agent: &Agent, limit: usize) -> Vec<StackFrame> {
    agent
        .execution_context_stack
        .iter()
        .rev()
        .filter(|context| is_frame(context))
        .take(limit)
        .map(frame_of)
        .collect()
}

/// Whether an execution context is a frame a host sees.
///
/// The realm's bootstrap context is the one context with no function, no script
/// or module and no source; it is pushed once and never popped, so it would
/// otherwise be the outermost frame of every trace. Eval code, which shares no
/// such shape (`source` is set), stays a frame.
fn is_frame(context: &ExecutionContext) -> bool {
    context.function.is_some() || context.script_or_module.is_some() || context.source.is_some()
}

/// The frame an execution context reports.
fn frame_of(context: &ExecutionContext) -> StackFrame {
    StackFrame {
        function_name: context.function.as_ref().and_then(|value| {
            let ValueKind::Function(function) = value.kind() else {
                return None;
            };
            function.name.as_ref().map(|name| name.to_string_lossy())
        }),
        script_name: script_name(context),
        // No position is tracked per activation; see the module header.
        line: 0,
        column: 0,
        is_eval: false,
        is_constructor: false,
        is_wasm: false,
        is_user_javascript: true,
    }
}

/// The name of the code a frame is running.
///
/// A module carries the name its host compiled it under; a classic script
/// carries none, because the engine parses one from text alone (`parse_script`
/// takes a source and a realm) and the host's `ScriptOrigin` is not threaded
/// through the eval path — an engine gap, recorded in `.notes/embedding.md`.
fn script_name(context: &ExecutionContext) -> Option<String> {
    match &context.script_or_module {
        Some(ScriptOrModule::Module(module)) => {
            module.name.as_ref().map(|name| name.to_string_lossy())
        }
        Some(ScriptOrModule::Script(_)) | None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bootstrap context is not a frame: it is pushed once and never popped,
    /// so without this rule every trace would end with it.
    #[test]
    fn the_bootstrap_context_is_not_a_frame() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        assert!(
            stack_frames(&agent, 10).is_empty(),
            "the realm's own context is not a frame"
        );
    }

    /// A capture nobody holds is dropped by the next collection. The frames live
    /// under the box address of the object the capture minted, and the compaction
    /// hook prunes a dead one — without that, the table would grow with every
    /// trace a host ever took, which is the problem §12 item 7 records for the
    /// bridge's position table.
    #[test]
    fn a_dropped_capture_is_pruned_by_a_collection() {
        let mut isolate = Isolate::new();
        let _context = Context::new(&mut isolate).expect("context");
        {
            let _capture = isolate.capture_stack(10).expect("a capture");
            assert_eq!(isolate.agent.stack_traces.borrow().len(), 1);
        }

        // The host stopped holding it. What makes the assert below about the
        // *pruning* rather than about reachability is that the compaction hook's
        // dead set comes from a precise mark: a stale stack word cannot keep the
        // capture's object alive, which is the same property the weak tables rely
        // on.
        isolate.agent.collect_garbage();
        assert!(
            isolate.agent.stack_traces.borrow().is_empty(),
            "a capture the host stopped holding is pruned"
        );
    }

    /// A limit of zero is a refusal rather than an empty capture, and there is no
    /// stack to report outside a realm.
    #[test]
    fn a_zero_limit_is_refused() {
        let mut isolate = Isolate::new();
        let _context = Context::new(&mut isolate).expect("context");
        assert!(isolate.capture_stack(0).is_none());
        assert!(isolate.capture_stack(1).is_some());
    }
}
