//! A Chrome DevTools Protocol facade over Slag's pause protocol.
//!
//! The engine's side is `.notes/chromium-facade.md` section 14.4: a session
//! attaches, breakpoints are source offsets, a stop is an outcome that is parked
//! and resumed, and an evaluation runs against the halted frame. This crate is
//! the protocol layer over that: it owns the script registry the protocol speaks
//! in (script ids and URLs), resolves `(url, line, column)` breakpoints to the
//! engine's offset-matched debug points, and turns stops into
//! `Debugger.paused`/`Debugger.resumed` events.
//!
//! What it does not do yet, because the engine cannot: report a remote object's
//! class name (only `typeof`) and invoke an accessor's getter while listing. A
//! stop reports every parked activation as a call frame, `stepOver`/`stepInto`/
//! `stepOut` stop at the next statement of the halted activation, a deeper one or
//! an outer one, `evaluateOnCallFrame` evaluates against the frame its
//! `callFrameId` names, and each frame carries a scope chain that
//! `Runtime.getProperties` reads binding by binding. A `stepInto` a generator or
//! a builtin callback's body is deferred, because those runs cannot be parked.
//!
//! The host drives it as Chrome does: register each script it compiles, feed
//! messages to [`Session::dispatch`], and ask [`Session::poll`] for the events a
//! run produced.
//!
//! A session is a *context group*: the embedder groups execution contexts
//! (Chromium groups by frame), and each world of a frame is a context of its
//! own. [`Session::dispatch_group`] and [`Session::poll_group`] take the group's
//! [`Target`]s, so a URL breakpoint resolves in every context that compiled the
//! text and a stop is announced for the context it happened in. The
//! single-context forms (`dispatch`, `poll`, `add_script`) are the same code
//! with one target whose id is 1.

use std::collections::HashMap;

use serde_json::{json, Value as Json};
use slag::{Context, JsValue};

/// One execution context a session debugs, as a command sees it.
///
/// The protocol's session spans a *context group*: the embedder groups contexts
/// (Chromium groups by frame), and each world of a frame is a `v8::Context` of
/// its own. `id` is the `executionContextId` those worlds are named by, and
/// `context` is the engine handle a command that runs code or reads a halted
/// frame needs.
pub struct Target<'a> {
    pub id: u32,
    pub context: &'a mut Context,
}

impl<'a> Target<'a> {
    pub fn new(id: u32, context: &'a mut Context) -> Self {
        Self { id, context }
    }
}

/// One registered script: what the protocol's ids and URLs name.
struct Script {
    id: u32,
    url: String,
    source: String,
    /// The engine's identity for this text, which is what the breakpoint set
    /// names: an offset alone is ambiguous across scripts.
    identity: u64,
    /// The body's debug points as `(start, end)` source offsets, from the
    /// engine. A breakpoint resolves to the first point at or after the
    /// requested position, because the engine matches a point by its own offset
    /// exactly.
    points: Vec<(u32, u32)>,
    /// The execution context that compiled it. A group can hold the same URL in
    /// more than one context, so this is what keeps a stop, a breakpoint and a
    /// `scriptParsed` in the world they belong to.
    context: u32,
}

/// A breakpoint the session set, in every script its selector resolved to: one
/// URL can name a script compiled in more than one execution context.
struct Breakpoint {
    hits: Vec<Hit>,
}

/// Where a breakpoint resolved: the script and the engine's point in it.
struct Hit {
    script_id: u32,
    offset: u32,
}

/// The stop that has been announced and not resumed.
struct Announced {
    target: u32,
}

/// One uncaught exception, as the engine and the host know it. The engine
/// records the thrown value and where it happened; the stack is the host's,
/// because a host captures it where the value is created (a frame is gone by
/// the time the error reaches it).
pub struct ExceptionReport<'a> {
    /// The text the client shows: the host's message for the thrown value.
    pub message: &'a str,
    /// The script's URL or name, as the host names it.
    pub url: &'a str,
    /// The position this report names, with the 1-based line and column that
    /// `v8::Message` answers; the protocol counts both from zero.
    pub line: u32,
    pub column: u32,
    /// Whether the thrown value is an error object, which is what the
    /// protocol's `subtype` distinguishes.
    pub is_error: bool,
    /// The frames of the stack captured where the value was created, innermost
    /// first.
    pub frames: &'a [ExceptionFrame<'a>],
}

/// One frame of an [`ExceptionReport`].
pub struct ExceptionFrame<'a> {
    pub function_name: &'a str,
    pub url: &'a str,
    /// 1-based, or 0 for "no position".
    pub line: u32,
    pub column: u32,
}

/// The index of the target `id` names.
fn target_index(targets: &[Target<'_>], id: u32) -> Option<usize> {
    targets.iter().position(|target| target.id == id)
}

/// Why a dispatch did not produce a result.
enum DispatchError {
    /// The method exists but the message was unusable.
    Protocol(String),
    /// No such method.
    Unknown,
}

/// A CDP session over one context group.
pub struct Session {
    scripts: Vec<Script>,
    breakpoints: HashMap<String, Breakpoint>,
    next_breakpoint: u64,
    /// The `Debugger` domain is enabled.
    enabled: bool,
    /// The `Runtime` domain is enabled. An exception event belongs to a client
    /// that asked for it, which is the same rule the `Debugger` events follow.
    runtime: bool,
    /// The id the next exception report is named by, so `exceptionRevoked` can
    /// name the same one back.
    next_exception: u32,
    /// The stop that has been announced and not resumed. Two contexts can each
    /// have a parked run; a client drives one stop at a time, so the announced
    /// one is the target every frame-bound command applies to.
    paused: Option<Announced>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            scripts: Vec::new(),
            breakpoints: HashMap::new(),
            next_breakpoint: 0,
            enabled: false,
            runtime: false,
            next_exception: 0,
            paused: None,
        }
    }

    /// Register a script the host is about to run, answering its id and the
    /// events a session that is already enabled owes the client.
    ///
    /// Registration is a copy of the text: the debug points are the engine's
    /// work, and they are compiled on the first `Debugger.enable` (or here, if
    /// a client is already attached). A host that never connects a debugger
    /// therefore pays nothing for the registry, and a client that attaches
    /// later still hears about every script that already ran, which is what V8
    /// does. A body that does not compile is the host's compile error, not a
    /// debug event.
    pub fn add_script(
        &mut self,
        context: &Context,
        url: &str,
        source: &str,
    ) -> Result<(u32, Vec<String>), String> {
        self.add_script_in(1, context, url, source)
    }

    /// [`add_script`](Self::add_script) for a group: `execution_context_id` is
    /// the target that compiled the text. Script ids are the group's, so a
    /// client's `scriptId` names the same script in `scriptParsed`,
    /// `Debugger.paused` and `getScriptSource`.
    pub fn add_script_in(
        &mut self,
        execution_context_id: u32,
        context: &Context,
        url: &str,
        source: &str,
    ) -> Result<(u32, Vec<String>), String> {
        // The points are the engine's compile; a registry that no client is
        // listening to does not need them yet (`Debugger.enable` fills them).
        let points = if self.enabled {
            context.debug_points(source).map_err(|e| e.message)?
        } else {
            Vec::new()
        };
        let identity = context.script_identity(source);
        let id = self.scripts.len() as u32 + 1;
        self.scripts.push(Script {
            id,
            url: url.to_string(),
            source: source.to_string(),
            identity,
            points,
            context: execution_context_id,
        });
        let script = self.scripts.last().expect("just pushed");
        let events = if self.enabled {
            vec![script_parsed(script)]
        } else {
            Vec::new()
        };
        Ok((id, events))
    }

    /// Drop what belongs to one client: the domains it enabled, its
    /// breakpoints, and the stop it is looking at. The script registry stays,
    /// so a client that reattaches to a running page still hears about the
    /// scripts that already ran.
    pub fn reset(&mut self) {
        self.enabled = false;
        self.runtime = false;
        self.paused = None;
        self.breakpoints.clear();
    }

    /// The command id `message` answers, as [`dispatch`](Self::dispatch) reads
    /// it, or `None` when it names none.
    ///
    /// A transport needs this because `V8Inspector::Channel` separates the
    /// answer to a command from an event, and the protocol carries it in the
    /// request. Chrome numbers its commands; an id of another type is echoed
    /// in the response but is not one this can name.
    pub fn call_id(message: &str) -> Option<i64> {
        serde_json::from_str::<Json>(message)
            .ok()?
            .get("id")?
            .as_i64()
    }

    /// Whether this session answers `method` at all
    /// (`V8InspectorSession::canDispatchMethod`), which is how a host decides
    /// whether a message belongs to the debugger or to its own agents.
    pub fn can_dispatch(method: &str) -> bool {
        matches!(
            method,
            "Runtime.enable"
                | "Runtime.disable"
                | "Runtime.getProperties"
                | "Runtime.releaseObject"
                | "Runtime.releaseObjectGroup"
                | "Runtime.evaluate"
                | "Debugger.enable"
                | "Debugger.disable"
                | "Debugger.setBreakpointByUrl"
                | "Debugger.removeBreakpoint"
                | "Debugger.pause"
                | "Debugger.resume"
                | "Debugger.stepOver"
                | "Debugger.stepInto"
                | "Debugger.stepOut"
                | "Debugger.evaluateOnCallFrame"
                | "Debugger.getScriptSource"
        )
    }

    /// Handle one protocol message against a single execution context.
    ///
    /// The answer is the events the command caused and then its response, as a
    /// `paused`/`resumed` pair arrives before the response to the command that
    /// caused it.
    pub fn dispatch(&mut self, context: &mut Context, message: &str) -> Vec<String> {
        self.dispatch_group(&mut [Target::new(1, context)], message)
    }

    /// [`dispatch`](Self::dispatch) over a context group: the commands that
    /// change engine state apply to every target, and the ones that act on a
    /// halted run apply to the target that is parked.
    pub fn dispatch_group(&mut self, targets: &mut [Target<'_>], message: &str) -> Vec<String> {
        let Ok(request) = serde_json::from_str::<Json>(message) else {
            return vec![error(None, -32700, "parse error")];
        };
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Json::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Json::Null);
        match self.handle(targets, method, &params) {
            Ok((result, events)) => {
                let mut out = events;
                out.push(response(id, result));
                out
            }
            Err(DispatchError::Protocol(message)) => vec![error(id, -32000, &message)],
            Err(DispatchError::Unknown) => vec![error(id, -32601, "method not found")],
        }
    }

    /// The events a run produced: a `Debugger.paused` for a stop that has not
    /// been announced yet, and nothing otherwise.
    pub fn poll(&mut self, context: &Context) -> Vec<String> {
        if !self.enabled || self.paused.is_some() {
            return Vec::new();
        }
        self.announce(1, context).unwrap_or_default()
    }

    /// [`poll`](Self::poll) over a context group: the first target with a parked
    /// run answers, because a client drives one stop at a time.
    pub fn poll_group(&mut self, targets: &mut [Target<'_>]) -> Vec<String> {
        if !self.enabled || self.paused.is_some() {
            return Vec::new();
        }
        for target in targets.iter_mut() {
            if let Some(events) = self.announce(target.id, target.context) {
                return events;
            }
        }
        Vec::new()
    }

    /// The `Debugger.paused` for target `id`'s parked run, when it has one.
    fn announce(&mut self, id: u32, context: &Context) -> Option<Vec<String>> {
        let info = context.paused()?;
        let offset = info.position.map(|span| span.start).unwrap_or(0);
        let (stopped_script, line, column) = self.location_of(id, offset).unwrap_or((1, 0, 0));
        let reason = match info.reason {
            slag::PauseReason::Breakpoint => "breakpoint",
            slag::PauseReason::Requested => "other",
            slag::PauseReason::Step => "step",
        };
        let hit = self.breakpoints_hit_at(id, offset);
        let call_frames = self.call_frames(id, context, stopped_script, line, column);
        self.paused = Some(Announced { target: id });
        let mut params = json!({
            "callFrames": call_frames,
            "reason": reason,
        });
        if !hit.is_empty() {
            params["hitBreakpoints"] = json!(hit);
        }
        Some(vec![event("Debugger.paused", params)])
    }

    /// The breakpoints whose point is `offset` in the target `id` names: a stop
    /// in one world must not report a breakpoint that belongs to another.
    fn breakpoints_hit_at(&self, target: u32, offset: u32) -> Vec<String> {
        self.breakpoints
            .iter()
            .filter(|(_, breakpoint)| {
                breakpoint
                    .hits
                    .iter()
                    .any(|hit| hit.offset == offset && self.in_target(hit.script_id, target))
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Whether the script `script_id` names was compiled by the target `target`.
    fn in_target(&self, script_id: u32, target: u32) -> bool {
        self.scripts
            .iter()
            .any(|script| script.id == script_id && script.context == target)
    }

    /// The target a command that acts on a halted run applies to: the announced
    /// one while it is parked, else the first parked one.
    fn stopped_index(&self, targets: &[Target<'_>]) -> Option<usize> {
        if let Some(announced) = &self.paused {
            if let Some(index) = target_index(targets, announced.target) {
                if targets[index].context.paused().is_some() {
                    return Some(index);
                }
            }
        }
        targets
            .iter()
            .position(|target| target.context.paused().is_some())
    }

    /// Continue every parked run in the group, answering whether there was one.
    fn resume_all(&self, targets: &mut [Target<'_>]) -> bool {
        let mut resumed = false;
        for target in targets.iter_mut() {
            if target.context.paused().is_none() {
                continue;
            }
            resumed = true;
            // A run that throws while finishing reports that to whoever started
            // it (`Script::Run`), not to the `Debugger.resume` that continued
            // it, so the error is not this answer's. Nothing is pending before a
            // resume, so nothing is left pending by one.
            let _ = target.context.resume_paused();
        }
        resumed
    }

    /// The parked activations as protocol call frames, innermost first.
    ///
    /// The engine reports each frame's own function name, script name and site,
    /// so the caller frames point at the call the stop interrupted. A frame
    /// belongs to a script of the *stopped target*: two worlds can run the same
    /// URL, and a frame must name the one it is in. A frame the session has no
    /// script for answers the script the stop resolved in, which is the script
    /// the run is executing.
    fn call_frames(
        &self,
        target: u32,
        context: &Context,
        script: u32,
        line: u32,
        column: u32,
    ) -> Json {
        let frames = context.agent().paused_frames();
        if frames.is_empty() {
            return json!([{
                "callFrameId": "frame-1",
                "functionName": "",
                "location": {
                    "scriptId": script.to_string(),
                    "lineNumber": line,
                    "columnNumber": column,
                },
                "scopeChain": [],
                "this": {"type": "undefined"},
            }]);
        }
        let call_frames: Vec<Json> = frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let script_id = frame
                    .script_name
                    .as_ref()
                    .and_then(|name| {
                        self.scripts
                            .iter()
                            .find(|script| script.context == target && &script.url == name)
                    })
                    .map_or(script, |script| script.id);
                // The engine reports each frame's own site, in the source text
                // its spans index, so no point lookup is needed here. A frame
                // with neither answers -1, which is what the protocol uses for
                // "unknown" rather than the first line of the file.
                let (line_number, column_number) = if frame.line == 0 {
                    (-1, -1)
                } else {
                    (
                        frame.line.saturating_sub(1) as i64,
                        frame.column.saturating_sub(1) as i64,
                    )
                };
                json!({
                    "callFrameId": format!("frame-{}", index + 1),
                    "functionName": frame.function_name.as_deref().unwrap_or(""),
                    "location": {
                        "scriptId": script_id.to_string(),
                        "lineNumber": line_number,
                        "columnNumber": column_number,
                    },
                    "scopeChain": self.scope_chain(context, index),
                    "this": {"type": "undefined"},
                })
            })
            .collect();
        Json::Array(call_frames)
    }

    /// One frame's scope chain: the activation's scopes, innermost first, each
    /// named by an id the client resolves through `Runtime.getProperties`.
    ///
    /// The id carries the level and the scope's index in the chain rather than a
    /// reference the session has to keep alive, so a stop holds no roots of its
    /// own: a scope is re-derived from the parked run on every read.
    fn scope_chain(&self, context: &Context, level: usize) -> Json {
        let Ok(scopes) = context.paused_scopes(level) else {
            return Json::Array(Vec::new());
        };
        Json::Array(
            scopes
                .iter()
                .enumerate()
                .map(|(index, scope)| {
                    let kind = scope_type(scope.kind);
                    json!({
                        "type": kind,
                        "object": {
                            "type": "object",
                            "className": "Object",
                            "description": kind,
                            "objectId": format!("scope:{level}:{index}"),
                        },
                    })
                })
                .collect(),
        )
    }

    /// `Runtime.getProperties`: a scope's bindings, or a value's own properties.
    ///
    /// `objectId` is a path rather than a session-held handle: a scope
    /// (`scope:<level>:<index>`), a binding inside it, or a property of one,
    /// with each step of the path escaped so a name may contain the separator.
    fn get_properties(
        &self,
        context: &mut Context,
        params: &Json,
    ) -> Result<(Json, Vec<String>), DispatchError> {
        let object_id = params["objectId"].as_str().unwrap_or("");
        let mut parts = object_id.split('/');
        let root = parts.next().unwrap_or("");
        let path: Vec<&str> = parts.collect();
        let (level, index) = parse_scope_root(root)?;
        if path.is_empty() {
            let scopes = context
                .paused_scopes(level)
                .map_err(|error| DispatchError::Protocol(error.message))?;
            let Some(scope) = scopes.get(index) else {
                return Err(DispatchError::Protocol(format!(
                    "no scope {index} in call frame {level}"
                )));
            };
            let result: Vec<Json> = scope
                .bindings
                .iter()
                .map(|binding| {
                    let child = format!("{object_id}/{}", escape(&binding.name));
                    let mut descriptor = json!({
                        "name": binding.name,
                        "configurable": false,
                        "enumerable": true,
                        "isOwn": true,
                        "writable": false,
                    });
                    match &binding.value {
                        Some(value) => {
                            descriptor["value"] = remote_object(value, Some(child));
                        }
                        None => {
                            // Still in its temporal dead zone: the engine knows
                            // the name and has nothing to answer.
                            descriptor["value"] = json!({"type": "undefined"});
                        }
                    }
                    descriptor
                })
                .collect();
            return Ok((json!({ "result": result }), Vec::new()));
        }
        let value = self
            .resolve(context, level, index, &path)
            .map_err(DispatchError::Protocol)?;
        let properties = context
            .own_properties(value)
            .map_err(|error| DispatchError::Protocol(error.message))?;
        let result: Vec<Json> = properties
            .iter()
            .map(|property| {
                let key = property
                    .name
                    .clone()
                    .or_else(|| property.symbol.clone())
                    .unwrap_or_default();
                let mut descriptor = json!({
                    "name": key,
                    "configurable": property.configurable,
                    "enumerable": property.enumerable,
                    "isOwn": true,
                    "writable": property.writable,
                });
                if let Some(symbol) = &property.symbol {
                    descriptor["symbol"] = remote_object(&JsValue::string(symbol.clone()), None);
                }
                if let Some(value) = &property.value {
                    let child = format!("{object_id}/{}", escape(&key));
                    descriptor["value"] = remote_object(value, Some(child));
                }
                descriptor
            })
            .collect();
        Ok((json!({ "result": result }), Vec::new()))
    }

    /// Walk an object id's path: the named binding of a scope, then a property
    /// per remaining step.
    fn resolve(
        &self,
        context: &mut Context,
        level: usize,
        index: usize,
        path: &[&str],
    ) -> Result<JsValue, String> {
        let Some((first, rest)) = path.split_first() else {
            return Err("an object id names no binding".into());
        };
        let scopes = context
            .paused_scopes(level)
            .map_err(|error| error.message)?;
        let Some(scope) = scopes.get(index) else {
            return Err(format!("no scope {index} in call frame {level}"));
        };
        let name = unescape(first);
        let Some(binding) = scope.bindings.iter().find(|binding| binding.name == name) else {
            return Err(format!("no binding {name} in scope {index}"));
        };
        let mut value = binding
            .value
            .clone()
            .ok_or_else(|| format!("the binding {name} is not initialized"))?;
        for step in rest {
            let key = JsValue::string(unescape(step));
            value = context
                .get_property(value, key)
                .map_err(|error| error.message)?;
        }
        Ok(value)
    }

    fn handle(
        &mut self,
        targets: &mut [Target<'_>],
        method: &str,
        params: &Json,
    ) -> Result<(Json, Vec<String>), DispatchError> {
        match method {
            "Runtime.enable" => {
                self.runtime = true;
                Ok((json!({}), Vec::new()))
            }
            "Runtime.disable" => {
                self.runtime = false;
                Ok((json!({}), Vec::new()))
            }
            "Runtime.getProperties" => {
                let Some(index) = self.stopped_index(targets) else {
                    return Err(DispatchError::Protocol("no run is halted".into()));
                };
                self.get_properties(targets[index].context, params)
            }
            // The object ids this session hands out name a position in the
            // parked run, not a handle it holds, so there is nothing to release.
            "Runtime.releaseObject" | "Runtime.releaseObjectGroup" => Ok((json!({}), Vec::new())),
            "Runtime.evaluate" => {
                let expression = params["expression"].as_str().unwrap_or("");
                // `contextId` names the target; a client that omits it means the
                // one it is looking at, which is the halted one when there is
                // one.
                let index = params
                    .get("contextId")
                    .and_then(Json::as_u64)
                    .and_then(|id| target_index(targets, id as u32))
                    .or_else(|| self.stopped_index(targets))
                    .unwrap_or(0);
                let Some(target) = targets.get_mut(index) else {
                    return Err(DispatchError::Protocol("no execution context".into()));
                };
                Ok((evaluation(target.context.eval(expression)), Vec::new()))
            }
            "Debugger.enable" => {
                // Attaching is per engine context, and a group is what the
                // protocol enables: a breakpoint has to be live in every world.
                for target in targets.iter_mut() {
                    target
                        .context
                        .set_debug_attached(true)
                        .map_err(|error| DispatchError::Protocol(error.message))?;
                }
                self.enabled = true;
                // The points a breakpoint resolves against are the ones the run
                // will carry, so they are compiled before the client can ask
                // for a location. A script registered while no client was
                // attached has none yet; one registered under a session already
                // has them.
                self.fill_points(targets);
                // A client that enables after scripts were registered expects
                // their `scriptParsed` events then.
                let events = self.scripts.iter().map(script_parsed).collect();
                Ok((json!({}), events))
            }
            "Debugger.disable" => {
                self.paused = None;
                let mut events = Vec::new();
                if self.resume_all(targets) {
                    events.push(event("Debugger.resumed", json!({})));
                }
                for target in targets.iter_mut() {
                    target
                        .context
                        .set_debug_attached(false)
                        .map_err(|error| DispatchError::Protocol(error.message))?;
                    target.context.set_script_breakpoints(&[]);
                }
                self.enabled = false;
                self.breakpoints.clear();
                Ok((json!({}), events))
            }
            "Debugger.setBreakpointByUrl" => self.set_breakpoint(targets, params),
            "Debugger.removeBreakpoint" => {
                let id = params["breakpointId"].as_str().unwrap_or("");
                if self.breakpoints.remove(id).is_none() {
                    return Err(DispatchError::Protocol(format!("no breakpoint {id}")));
                }
                self.push_breakpoints(targets);
                Ok((json!({}), Vec::new()))
            }
            "Debugger.pause" => {
                // A pause is group-wide: whichever context runs next stops. The
                // engine's request is one-shot per context and nothing clears
                // another context's ticket, so a context that runs later stops
                // as well; `poll` reports the first and the client resumes the
                // rest.
                for target in targets.iter_mut() {
                    target.context.request_pause();
                }
                Ok((json!({}), Vec::new()))
            }
            "Debugger.resume" => {
                self.paused = None;
                // Nothing parked is not an error: the client may resume after a
                // stop it already handled.
                let events = if self.resume_all(targets) {
                    vec![event("Debugger.resumed", json!({}))]
                } else {
                    Vec::new()
                };
                Ok((json!({}), events))
            }
            "Debugger.stepOver" | "Debugger.stepInto" | "Debugger.stepOut" => {
                let mode = match method {
                    "Debugger.stepInto" => slag::StepMode::Into,
                    "Debugger.stepOver" => slag::StepMode::Over,
                    _ => slag::StepMode::Out,
                };
                // A step is relative to the halted activation, so only its
                // context is armed: arming every context would make an
                // unrelated script stop later.
                let Some(index) = self.stopped_index(targets) else {
                    return Err(DispatchError::Protocol("no run is halted".into()));
                };
                targets[index].context.request_step(mode);
                self.paused = None;
                // The resume itself reports the stop it produced through the
                // paused error; `poll` is what announces it, so a step that
                // lands immediately emits `resumed` then `paused` in order.
                let events = if self.resume_all(targets) {
                    vec![event("Debugger.resumed", json!({}))]
                } else {
                    Vec::new()
                };
                Ok((json!({}), events))
            }
            "Debugger.evaluateOnCallFrame" => {
                let expression = params["expression"].as_str().unwrap_or("");
                // `frame-N` is the N-th parked activation of the halted target,
                // innermost first (`Debugger.paused`'s `callFrames`), or the
                // halted one when the client names nothing.
                let level = params["callFrameId"]
                    .as_str()
                    .and_then(|id| id.strip_prefix("frame-"))
                    .and_then(|index| index.parse::<usize>().ok())
                    .map_or(0, |index| index.saturating_sub(1));
                let Some(index) = self.stopped_index(targets) else {
                    return Err(DispatchError::Protocol("no run is halted".into()));
                };
                Ok((
                    evaluation(targets[index].context.evaluate_paused_at(level, expression)),
                    Vec::new(),
                ))
            }
            "Debugger.getScriptSource" => {
                let script_id = params["scriptId"]
                    .as_str()
                    .and_then(|id| id.parse::<u32>().ok())
                    .unwrap_or(0);
                let Some(script) = self.scripts.iter().find(|s| s.id == script_id) else {
                    return Err(DispatchError::Protocol(format!("no script {script_id}")));
                };
                Ok((json!({ "scriptSource": script.source }), Vec::new()))
            }
            _ => Err(DispatchError::Unknown),
        }
    }

    fn set_breakpoint(
        &mut self,
        targets: &mut [Target<'_>],
        params: &Json,
    ) -> Result<(Json, Vec<String>), DispatchError> {
        let url = params["url"].as_str().unwrap_or("");
        let line = params["lineNumber"].as_u64().unwrap_or(0) as u32;
        let column = params["columnNumber"].as_u64().unwrap_or(0) as u32;
        // A URL can name a script in more than one execution context: a world
        // that compiled the same text is a breakpoint in each of them.
        let matches: Vec<(u32, String)> = self
            .scripts
            .iter()
            .filter(|script| script.url == url)
            .map(|script| (script.id, script.source.clone()))
            .collect();
        if matches.is_empty() {
            return Err(DispatchError::Protocol(format!("no script for {url}")));
        }
        let id = self.mint_breakpoint();
        let mut hits = Vec::new();
        let mut locations = Vec::new();
        for (script_id, source) in matches {
            // The offset a breakpoint stops at is the first *code* position at
            // or after the request, not the line's first character: the engine
            // matches a debug point by the span it starts at, and a statement's
            // span starts at its first token. That is also what makes a
            // breakpoint inside a function body work, because the span offsets
            // of every body are offsets into this one text.
            let Some(offset) = code_offset_of(&source, line, column) else {
                continue;
            };
            let (resolved_line, resolved_column) = position_of(&source, offset);
            hits.push(Hit { script_id, offset });
            locations.push(json!({
                "scriptId": script_id.to_string(),
                "lineNumber": resolved_line,
                "columnNumber": resolved_column,
            }));
        }
        // Chrome allows a breakpoint with no location yet: it answers an id and
        // no locations, and the client keeps it. That is also the answer when
        // the line carries no code in any script the URL names.
        if !hits.is_empty() {
            self.breakpoints.insert(id.clone(), Breakpoint { hits });
            self.push_breakpoints(targets);
        }
        Ok((
            json!({ "breakpointId": id, "locations": locations }),
            Vec::new(),
        ))
    }

    /// Hand every context the whole set, each entry with the identity of the
    /// script it resolved in: the engine matches a point by offset, so removing
    /// one breakpoint means replacing the list.
    fn push_breakpoints(&self, targets: &mut [Target<'_>]) {
        let pairs: Vec<(u64, u32)> = self
            .breakpoints
            .values()
            .flat_map(|breakpoint| breakpoint.hits.iter())
            .filter_map(|hit| {
                let script = self
                    .scripts
                    .iter()
                    .find(|script| script.id == hit.script_id)?;
                Some((script.identity, hit.offset))
            })
            .collect();
        // The set is per engine context and every entry names its script by the
        // text's identity, so each context takes all of it: a context matches
        // the entries whose text it runs. That is the URL semantics the protocol
        // asks for, and it is also why a script a context compiles later, after
        // the breakpoint was resolved elsewhere, is still breakable.
        for target in targets.iter_mut() {
            target.context.set_script_breakpoints(&pairs);
        }
    }

    /// Compile the points of every script that has none, for the targets that
    /// own it. A failure leaves the script without points, which costs it
    /// breakpoints and nothing else: the text is already known to parse, since
    /// the host compiled it.
    fn fill_points(&mut self, targets: &mut [Target<'_>]) {
        for target in targets.iter_mut() {
            for script in self.scripts.iter_mut() {
                if script.context != target.id || !script.points.is_empty() {
                    continue;
                }
                if let Ok(points) = target.context.debug_points(&script.source) {
                    script.points = points;
                }
            }
        }
    }

    fn mint_breakpoint(&mut self) -> String {
        self.next_breakpoint += 1;
        format!("1:{}:0", self.next_breakpoint)
    }

    /// The `Runtime.exceptionThrown` event for a report, with the id that names
    /// it to `exceptionRevoked`. `None` when the client did not enable
    /// `Runtime`: V8 reports an uncaught error only to a client that asked.
    pub fn exception(&mut self, report: &ExceptionReport<'_>) -> Option<(u32, Vec<String>)> {
        if !self.runtime {
            return None;
        }
        self.next_exception += 1;
        let exception_id = self.next_exception;
        let mut details = json!({
            "exceptionId": exception_id,
            "text": report.message,
            // The protocol counts lines and columns from zero; the host counts
            // them from one, as `v8::Message` does.
            "lineNumber": report.line.saturating_sub(1),
            "columnNumber": report.column.saturating_sub(1),
            "url": report.url,
            "exception": if report.is_error {
                json!({"type": "object", "subtype": "error", "description": report.message})
            } else {
                json!({"type": "object", "description": report.message})
            },
        });
        if let Some(script) = self.scripts.iter().find(|script| script.url == report.url) {
            details["scriptId"] = json!(script.id.to_string());
        }
        if !report.frames.is_empty() {
            let frames: Vec<Json> = report
                .frames
                .iter()
                .map(|frame| {
                    // A frame's `scriptId` is the group's id for the script the
                    // frame runs, which is what connects it to `scriptParsed`.
                    let script_id = self
                        .scripts
                        .iter()
                        .find(|script| script.url == frame.url)
                        .map_or_else(String::new, |script| script.id.to_string());
                    json!({
                        "functionName": frame.function_name,
                        "scriptId": script_id,
                        "url": frame.url,
                        "lineNumber": frame.line.saturating_sub(1),
                        "columnNumber": frame.column.saturating_sub(1),
                    })
                })
                .collect();
            details["stackTrace"] = json!({ "callFrames": frames });
        }
        Some((
            exception_id,
            vec![event(
                "Runtime.exceptionThrown",
                json!({
                    "timestamp": timestamp_ms(),
                    "exceptionDetails": details,
                }),
            )],
        ))
    }

    /// `Runtime.exceptionRevoked`: the error a host reported is no longer
    /// uncaught (a `catch` or an `unhandledrejection` handler took it).
    pub fn revoke_exception(&mut self, exception_id: u32, reason: &str) -> Vec<String> {
        if !self.runtime {
            return Vec::new();
        }
        vec![event(
            "Runtime.exceptionRevoked",
            json!({ "exceptionId": exception_id, "reason": reason }),
        )]
    }

    /// The script and 0-based position a stop's offset belongs to in the target
    /// `target`: the breakpoint at that offset if there is one, else the first
    /// script of that target that carries a point there.
    fn location_of(&self, target: u32, offset: u32) -> Option<(u32, u32, u32)> {
        let at_offset = self
            .breakpoints
            .values()
            .flat_map(|breakpoint| breakpoint.hits.iter())
            .find(|hit| hit.offset == offset && self.in_target(hit.script_id, target))
            .map(|hit| hit.script_id);
        if let Some(script_id) = at_offset {
            let script = self.scripts.iter().find(|script| script.id == script_id)?;
            let (line, column) = position_of(&script.source, offset);
            return Some((script.id, line, column));
        }
        let script = self
            .scripts
            .iter()
            .filter(|script| script.context == target)
            .find(|script| script.points.iter().any(|(start, _)| *start == offset))?;
        let (line, column) = position_of(&script.source, offset);
        Some((script.id, line, column))
    }
}

/// A scope's protocol `type` name.
fn scope_type(kind: slag::PausedScopeKind) -> &'static str {
    match kind {
        slag::PausedScopeKind::Local => "local",
        slag::PausedScopeKind::Closure => "closure",
        slag::PausedScopeKind::Block => "block",
        slag::PausedScopeKind::Catch => "catch",
        slag::PausedScopeKind::With => "with",
        slag::PausedScopeKind::Global => "global",
        slag::PausedScopeKind::Module => "module",
    }
}

/// The `(level, index)` a scope object id names, from its `scope:<l>:<i>` root.
fn parse_scope_root(root: &str) -> Result<(usize, usize), DispatchError> {
    let mut parts = root.split(':');
    let tag = parts.next().unwrap_or("");
    let level = parts.next().and_then(|text| text.parse::<usize>().ok());
    let index = parts.next().and_then(|text| text.parse::<usize>().ok());
    match (tag, level, index, parts.next()) {
        ("scope", Some(level), Some(index), None) => Ok((level, index)),
        _ => Err(DispatchError::Protocol(format!(
            "object id {root} is not a scope"
        ))),
    }
}

/// Escape a path step, so a name may contain the separator.
fn escape(text: &str) -> String {
    text.replace('%', "%25").replace('/', "%2F")
}

/// The inverse of [`escape`].
fn unescape(text: &str) -> String {
    text.replace("%2F", "/").replace("%25", "%")
}

/// A CDP command's answer to an evaluation: a remote object, or the exception
/// details Chrome expects for one that threw.
fn evaluation(result: Result<JsValue, slag::JsError>) -> Json {
    match result {
        Ok(value) => json!({ "result": remote_object(&value, None) }),
        Err(error) => json!({
            "result": {"type": "object", "subtype": "error", "description": error.message},
            "exceptionDetails": {"text": error.message},
        }),
    }
}

/// The remote object a value maps to. Primitives carry their value; anything
/// else is named as an object, because the engine exposes no description or
/// property listing yet (`Runtime.getProperties` needs a scope model it does not
/// have).
/// A value as the protocol's `RemoteObject`.
///
/// `object_id` is the path a client expands the value through. A primitive
/// carries its value and needs none; an object carries one, because its own
/// properties are only reachable through the position it was found at.
fn remote_object(value: &JsValue, object_id: Option<String>) -> Json {
    if value.is_undefined() {
        return json!({ "type": "undefined" });
    }
    if value.is_null() {
        return json!({ "type": "object", "subtype": "null", "value": Json::Null });
    }
    if let Some(number) = value.as_number() {
        // JSON has no NaN/Infinity; Chrome names those as a string.
        let json_number = serde_json::Number::from_f64(number);
        return match json_number {
            Some(number) => json!({ "type": "number", "value": number }),
            None => json!({ "type": "number", "unserializableValue": number.to_string() }),
        };
    }
    if let Some(boolean) = value.as_boolean() {
        return json!({ "type": "boolean", "value": boolean });
    }
    if let Some(text) = value.as_string() {
        return json!({ "type": "string", "value": text });
    }
    let class = value.type_name();
    let mut object = json!({
        "type": if class == "function" { "function" } else { "object" },
        "className": class_of(class),
        "description": class,
    });
    if let Some(id) = object_id {
        object["objectId"] = json!(id);
    }
    object
}

/// The constructor name a remote object reports: the engine knows `typeof`, and
/// a class name would need a property read the protocol layer should not make
/// while listing a scope.
fn class_of(type_name: &str) -> &'static str {
    match type_name {
        "function" => "Function",
        "object" => "Object",
        _ => "Object",
    }
}

fn script_parsed(script: &Script) -> String {
    let (end_line, end_column) = position_of(&script.source, script.source.len() as u32);
    event(
        "Debugger.scriptParsed",
        json!({
            "scriptId": script.id.to_string(),
            "url": script.url,
            "startLine": 0,
            "startColumn": 0,
            "endLine": end_line,
            "endColumn": end_column,
            "executionContextId": script.context,
            "hash": "",
        }),
    )
}

fn response(id: Option<Json>, result: Json) -> String {
    json!({ "id": id.unwrap_or(Json::Null), "result": result }).to_string()
}

fn error(id: Option<Json>, code: i64, message: &str) -> String {
    json!({
        "id": id.unwrap_or(Json::Null),
        "error": { "code": code, "message": message },
    })
    .to_string()
}

fn event(method: &str, params: Json) -> String {
    json!({ "method": method, "params": params }).to_string()
}

/// The wall-clock time the protocol's `timestamp` carries, in milliseconds.
fn timestamp_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1000.0)
}

/// The source offset of a 0-based line and column. Lines and columns are counted
/// in characters, which is exact for ASCII (the usual case for a breakpoint a
/// person sets) and an approximation for text whose UTF-16 units are not one
/// character each.
fn offset_of(source: &str, line: u32, column: u32) -> u32 {
    let mut current_line = 0;
    let mut offset = 0;
    for (index, character) in source.char_indices() {
        if current_line == line {
            let line_start = index;
            let end = source[line_start..]
                .char_indices()
                .nth(column as usize)
                .map_or(source.len(), |(at, _)| line_start + at);
            return end as u32;
        }
        if character == '\n' {
            current_line += 1;
        }
        offset = index + character.len_utf8();
    }
    offset as u32
}

/// The offset of the first code position at or after a 0-based line and column:
/// the request's own offset when it is already on code, else the next
/// non-whitespace character. `None` when the line has no code (past its end, or
/// blank), which is the case a breakpoint answers with no locations.
fn code_offset_of(source: &str, line: u32, column: u32) -> Option<u32> {
    let offset = offset_of(source, line, column) as usize;
    let rest = source.get(offset..)?;
    let skip = rest
        .char_indices()
        .take_while(|(_, character)| *character == ' ' || *character == '\t')
        .last()
        .map_or(0, |(at, character)| at + character.len_utf8());
    let at = offset + skip;
    let next = source.get(at..)?.chars().next()?;
    // A blank line, the end of the text, or the next line's start means the
    // request names nothing to break on.
    (!matches!(next, '\n' | '\r' | '\u{2028}' | '\u{2029}')).then_some(at as u32)
}

/// The 0-based line and column of a source offset: the inverse of [`offset_of`]
/// for ASCII, and the same approximation otherwise.
fn position_of(source: &str, offset: u32) -> (u32, u32) {
    let mut line = 0;
    let mut column = 0;
    for (index, character) in source.char_indices() {
        if index >= offset as usize {
            break;
        }
        if character == '\n' {
            line += 1;
            column = 0;
        } else {
            column += 1;
        }
    }
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> (Context, Session) {
        let mut context = Context::new().expect("a context");
        let mut session = Session::new();
        let enabled = session.dispatch(&mut context, r#"{"id":1,"method":"Debugger.enable"}"#);
        assert_eq!(enabled.len(), 1);
        (context, session)
    }

    const SOURCE: &str = "var a = 1;\na = a + 2;\na;\n";

    /// Register the source, set a breakpoint on line 1, and run it, answering
    /// the stop's events.
    fn run_to_breakpoint(context: &mut Context, session: &mut Session) -> Vec<String> {
        let (id, _) = session.add_script(context, "app.js", SOURCE).unwrap();
        assert_eq!(id, 1);
        let answer = session.dispatch(
            context,
            r#"{"id":2,"method":"Debugger.setBreakpointByUrl","params":{"url":"app.js","lineNumber":1,"columnNumber":0}}"#,
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        assert_eq!(
            response["result"]["locations"][0]["lineNumber"], 1,
            "the breakpoint resolved to the statement on line 1: {response}"
        );
        let error = context.agent_mut().run_script(SOURCE).unwrap_err();
        assert_eq!(error.message, "execution paused at a breakpoint");
        session.poll(context)
    }

    #[test]
    fn a_line_breakpoint_stops_the_run_and_reports_where() {
        let (mut context, mut session) = session();
        let events = run_to_breakpoint(&mut context, &mut session);
        assert_eq!(events.len(), 1, "{events:?}");
        let paused: Json = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(paused["method"], "Debugger.paused");
        assert_eq!(paused["params"]["reason"], "breakpoint");
        let frame = &paused["params"]["callFrames"][0];
        assert_eq!(frame["location"]["scriptId"], "1");
        assert_eq!(frame["location"]["lineNumber"], 1);
        assert!(paused["params"]["hitBreakpoints"].is_array());
    }

    #[test]
    fn evaluate_on_the_paused_frame_reads_the_halted_scope() {
        let (mut context, mut session) = session();
        run_to_breakpoint(&mut context, &mut session);
        let answer = session.dispatch(
            &mut context,
            r#"{"id":3,"method":"Debugger.evaluateOnCallFrame","params":{"callFrameId":"frame-1","expression":"a + 41"}}"#,
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        assert_eq!(response["result"]["result"]["value"], 42.0, "{response}");
    }

    #[test]
    fn resume_continues_the_run_and_reports_it() {
        let (mut context, mut session) = session();
        run_to_breakpoint(&mut context, &mut session);
        let answer = session.dispatch(&mut context, r#"{"id":4,"method":"Debugger.resume"}"#);
        let answers: Vec<Json> = answer
            .iter()
            .map(|text| serde_json::from_str(text).unwrap())
            .collect();
        assert_eq!(answers[0]["method"], "Debugger.resumed");
        assert!(answers[1]["result"].is_object());
        assert!(
            session.poll(&context).is_empty(),
            "the run is not parked any more"
        );
    }

    #[test]
    fn a_second_stop_is_reported_once() {
        let (mut context, mut session) = session();
        let events = run_to_breakpoint(&mut context, &mut session);
        assert_eq!(events.len(), 1);
        assert!(
            session.poll(&context).is_empty(),
            "the same stop is announced once"
        );
    }

    #[test]
    fn a_stop_inside_a_called_function_reports_both_frames() {
        let (mut context, mut session) = session();
        let source = "function f() {\n  globalThis.hit = 1;\n  return 2;\n}\nvar v = f();\n";
        session.add_script(&context, "app.js", source).unwrap();
        session.dispatch(
            &mut context,
            r#"{"id":2,"method":"Debugger.setBreakpointByUrl","params":{"url":"app.js","lineNumber":1,"columnNumber":0}}"#,
        );
        let error = context.agent_mut().run_script(source).unwrap_err();
        assert_eq!(error.message, "execution paused at a breakpoint");
        let events = session.poll(&context);
        assert_eq!(events.len(), 1, "{events:?}");
        let paused: Json = serde_json::from_str(&events[0]).unwrap();
        let frames = paused["params"]["callFrames"].as_array().unwrap();
        assert_eq!(
            frames.len(),
            2,
            "the callee's activation and the script's: {paused}"
        );
        assert_eq!(frames[0]["functionName"], "f");
        assert_eq!(frames[0]["location"]["lineNumber"], 1);
        assert_eq!(frames[0]["callFrameId"], "frame-1");
        assert_eq!(
            frames[1]["functionName"], "",
            "the script's own activation has no function name"
        );
        assert_eq!(
            frames[1]["location"]["lineNumber"], 4,
            "the caller frame points at the call the stop interrupted: {paused}"
        );
        assert_eq!(frames[1]["location"]["scriptId"], "1");
        // A resume finishes both activations.
        session.dispatch(&mut context, r#"{"id":3,"method":"Debugger.resume"}"#);
        assert_eq!(
            context.eval("globalThis.v").err().map(|e| e.message),
            None,
            "the run finished without a second stop"
        );
    }

    #[test]
    fn evaluate_on_call_frame_reads_the_frame_it_names() {
        let (mut context, mut session) = session();
        let source = "function f(x) {\n  globalThis.hit = 1;\n  return x;\n}\nvar v = f(7);\n";
        session.add_script(&context, "app.js", source).unwrap();
        session.dispatch(
            &mut context,
            r#"{"id":2,"method":"Debugger.setBreakpointByUrl","params":{"url":"app.js","lineNumber":1,"columnNumber":0}}"#,
        );
        assert!(context.agent_mut().run_script(source).is_err());
        session.poll(&context);
        // frame-1 is the halted activation, frame-2 its caller (the script).
        let answer = session.dispatch(
            &mut context,
            r#"{"id":3,"method":"Debugger.evaluateOnCallFrame","params":{"callFrameId":"frame-1","expression":"x * 2"}}"#,
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        assert_eq!(response["result"]["result"]["value"], 14.0, "{response}");
        let answer = session.dispatch(
            &mut context,
            r#"{"id":4,"method":"Debugger.evaluateOnCallFrame","params":{"callFrameId":"frame-2","expression":"typeof x"}}"#,
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        assert_eq!(
            response["result"]["result"]["value"], "undefined",
            "the caller has no `x`: {response}"
        );
    }

    #[test]
    fn a_scope_chain_reads_its_bindings_and_expands_a_value() {
        let (mut context, mut session) = session();
        // The property name contains the path separator, so the id a client
        // gets back has to escape it or expanding the value would walk one
        // step too far.
        let source = "function f(param) {\n  let local = { \"a/b\": { deep: 7 } };\n  globalThis.seen = local;\n  globalThis.hit = 1;\n}\nf(5);\n";
        session.add_script(&context, "app.js", source).unwrap();
        session.dispatch(
            &mut context,
            r#"{"id":2,"method":"Debugger.setBreakpointByUrl","params":{"url":"app.js","lineNumber":3,"columnNumber":0}}"#,
        );
        assert!(context.agent_mut().run_script(source).is_err());
        let events = session.poll(&context);
        let paused: Json = serde_json::from_str(&events[0]).unwrap();
        let scopes = paused["params"]["callFrames"][0]["scopeChain"]
            .as_array()
            .unwrap();
        assert_eq!(scopes[0]["type"], "local", "{paused}");
        let scope_id = scopes[0]["object"]["objectId"].as_str().unwrap();

        let answer = session.dispatch(
            &mut context,
            &format!(
                r#"{{"id":3,"method":"Runtime.getProperties","params":{{"objectId":"{scope_id}"}}}}"#
            ),
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        let properties = response["result"]["result"].as_array().unwrap();
        let find = |wanted: &str| {
            properties
                .iter()
                .find(|property| property["name"] == wanted)
                .unwrap_or_else(|| panic!("no binding {wanted}: {response}"))
        };
        assert_eq!(find("param")["value"]["value"], 5.0);
        let local = find("local");
        assert_eq!(local["value"]["type"], "object");
        let local_id = local["value"]["objectId"].as_str().unwrap();

        // The value the binding names is expandable through the same path.
        let answer = session.dispatch(
            &mut context,
            &format!(
                r#"{{"id":4,"method":"Runtime.getProperties","params":{{"objectId":"{local_id}"}}}}"#
            ),
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        let nested = &response["result"]["result"][0];
        assert_eq!(nested["name"], "a/b", "{response}");
        let nested_id = nested["value"]["objectId"].as_str().unwrap();
        assert!(
            nested_id.contains("%2F"),
            "the separator in a name is escaped: {nested_id}"
        );
        let answer = session.dispatch(
            &mut context,
            &format!(
                r#"{{"id":5,"method":"Runtime.getProperties","params":{{"objectId":"{nested_id}"}}}}"#
            ),
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        let deep = &response["result"]["result"][0];
        assert_eq!(deep["name"], "deep", "{response}");
        assert_eq!(deep["value"]["value"], 7.0);
    }

    #[test]
    fn a_breakpoint_in_one_script_does_not_stop_another() {
        let (mut context, mut session) = session();
        // Same length and the same offset for their one statement: an
        // offset-only breakpoint set would fire in both.
        let named = "globalThis.one = 1;";
        let other = "globalThis.two = 2;";
        session.add_script(&context, "a.js", named).unwrap();
        session.add_script(&context, "b.js", other).unwrap();
        session.dispatch(
            &mut context,
            r#"{"id":2,"method":"Debugger.setBreakpointByUrl","params":{"url":"a.js","lineNumber":0,"columnNumber":0}}"#,
        );
        assert!(
            context.agent_mut().run_script(other).is_ok(),
            "the other script runs to its end"
        );
        assert!(
            session.poll(&context).is_empty(),
            "a breakpoint in one script must not stop another"
        );
        assert!(context.agent_mut().run_script(named).is_err());
        let events = session.poll(&context);
        assert_eq!(events.len(), 1, "the script it names stops: {events:?}");
        let paused: Json = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(
            paused["params"]["callFrames"][0]["location"]["scriptId"],
            "1"
        );
    }

    #[test]
    fn an_unknown_method_is_refused_in_the_protocol_way() {
        let (mut context, mut session) = session();
        let answer = session.dispatch(
            &mut context,
            r#"{"id":7,"method":"Debugger.setPauseOnExceptions"}"#,
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn stepping_over_stops_at_the_next_line_in_the_same_frame() {
        let (mut context, mut session) = session();
        let source = "function f() {\n  globalThis.a = 1;\n  globalThis.b = 1;\n}\nf();\n";
        session.add_script(&context, "app.js", source).unwrap();
        session.dispatch(
            &mut context,
            r#"{"id":2,"method":"Debugger.setBreakpointByUrl","params":{"url":"app.js","lineNumber":1,"columnNumber":0}}"#,
        );
        assert!(context.agent_mut().run_script(source).is_err());
        assert!(session.poll(&context).len() == 1);

        let answer = session.dispatch(&mut context, r#"{"id":3,"method":"Debugger.stepOver"}"#);
        let answers: Vec<Json> = answer
            .iter()
            .map(|text| serde_json::from_str(text).unwrap())
            .collect();
        assert_eq!(answers[0]["method"], "Debugger.resumed");
        let events = session.poll(&context);
        assert_eq!(events.len(), 1, "the step stopped again: {events:?}");
        let paused: Json = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(paused["params"]["reason"], "step");
        let frame = &paused["params"]["callFrames"][0];
        assert_eq!(frame["functionName"], "f");
        assert_eq!(
            frame["location"]["lineNumber"], 2,
            "the step landed on the next statement, not in the script: {paused}"
        );
    }

    #[test]
    fn runtime_evaluate_answers_a_remote_object() {
        let (mut context, mut session) = session();
        let answer = session.dispatch(
            &mut context,
            r#"{"id":8,"method":"Runtime.evaluate","params":{"expression":"1 + 2"}}"#,
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        assert_eq!(response["result"]["result"]["type"], "number");
        assert_eq!(response["result"]["result"]["value"], 3.0);
    }

    #[test]
    fn a_script_registered_after_enable_gets_its_parsed_event() {
        let (context, mut session) = session();
        let (_, events) = session.add_script(&context, "late.js", "1;").unwrap();
        assert_eq!(events.len(), 1);
        let parsed: Json = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(parsed["method"], "Debugger.scriptParsed");
        assert_eq!(parsed["params"]["url"], "late.js");
    }

    #[test]
    fn disabling_releases_the_engine_session() {
        let (mut context, mut session) = session();
        run_to_breakpoint(&mut context, &mut session);
        let answer = session.dispatch(&mut context, r#"{"id":9,"method":"Debugger.disable"}"#);
        let answers: Vec<Json> = answer
            .iter()
            .map(|text| serde_json::from_str(text).unwrap())
            .collect();
        assert_eq!(answers[0]["method"], "Debugger.resumed");
        assert!(context.paused().is_none(), "the parked run is resumed");
    }

    #[test]
    fn a_breakpoint_past_the_last_statement_resolves_to_nothing() {
        let (mut context, mut session) = session();
        session.add_script(&context, "app.js", SOURCE).unwrap();
        let answer = session.dispatch(
            &mut context,
            r#"{"id":10,"method":"Debugger.setBreakpointByUrl","params":{"url":"app.js","lineNumber":99}}"#,
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        assert_eq!(
            response["result"]["locations"].as_array().map(Vec::len),
            Some(0)
        );
        assert!(response["result"]["breakpointId"].is_string());
    }

    /// A session over two execution contexts, which is what a context group is:
    /// a frame's main world and one isolated world, each a `v8::Context`.
    fn group() -> (Session, Context, Context) {
        (
            Session::new(),
            Context::new().expect("a context"),
            Context::new().expect("a context"),
        )
    }

    #[test]
    fn one_url_in_two_contexts_resolves_and_stops_in_each() {
        let (mut session, mut main, mut world) = group();
        let mut targets = [Target::new(1, &mut main), Target::new(2, &mut world)];
        // The debug points exist only while a session is attached, and the
        // protocol is what attaches every context of the group.
        session.dispatch_group(&mut targets, r#"{"id":0,"method":"Debugger.enable"}"#);
        // The same text in both worlds: one URL, one breakpoint, two scripts,
        // and the group mints the ids so a client can tell them apart.
        let (first, _) = session
            .add_script_in(1, targets[0].context, "app.js", SOURCE)
            .unwrap();
        let (second, _) = session
            .add_script_in(2, targets[1].context, "app.js", SOURCE)
            .unwrap();
        assert_eq!((first, second), (1, 2));

        let answer = session.dispatch_group(
            &mut targets,
            r#"{"id":1,"method":"Debugger.setBreakpointByUrl","params":{"url":"app.js","lineNumber":1,"columnNumber":0}}"#,
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        let locations = response["result"]["locations"].as_array().unwrap();
        assert_eq!(locations.len(), 2, "one per context: {response}");
        assert_eq!(locations[0]["scriptId"], "1");
        assert_eq!(locations[1]["scriptId"], "2");

        // A run in the second world stops on its own script, not the first's.
        let error = targets[1]
            .context
            .agent_mut()
            .run_script(SOURCE)
            .unwrap_err();
        assert_eq!(error.message, "execution paused at a breakpoint");
        assert!(
            targets[0].context.paused().is_none(),
            "the other world's run is untouched"
        );
        let events = session.poll_group(&mut targets);
        assert_eq!(events.len(), 1, "{events:?}");
        let paused: Json = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(
            paused["params"]["callFrames"][0]["location"]["scriptId"], "2",
            "{paused}"
        );

        // The evaluation runs in the world that stopped.
        let answer = session.dispatch_group(
            &mut targets,
            r#"{"id":2,"method":"Debugger.evaluateOnCallFrame","params":{"callFrameId":"frame-1","expression":"a + 41"}}"#,
        );
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        assert_eq!(response["result"]["result"]["value"], 42.0, "{response}");

        let answer = session.dispatch_group(&mut targets, r#"{"id":3,"method":"Debugger.resume"}"#);
        let answers: Vec<Json> = answer
            .iter()
            .map(|text| serde_json::from_str(text).unwrap())
            .collect();
        assert_eq!(answers[0]["method"], "Debugger.resumed");
        assert!(
            targets[1].context.paused().is_none(),
            "the resumed run finished"
        );
    }

    #[test]
    fn a_script_parsed_event_names_the_context_that_compiled_it() {
        let (mut session, mut main, mut world) = group();
        let mut targets = [Target::new(1, &mut main), Target::new(2, &mut world)];
        session.dispatch_group(&mut targets, r#"{"id":1,"method":"Debugger.enable"}"#);
        let (id, events) = session
            .add_script_in(2, targets[1].context, "world.js", "globalThis.w = 1;")
            .unwrap();
        assert_eq!(id, 1, "script ids are the group's");
        assert_eq!(events.len(), 1, "{events:?}");
        let parsed: Json = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(parsed["params"]["executionContextId"], 2);
    }

    #[test]
    fn runtime_evaluate_honours_the_context_it_names() {
        let (mut session, mut main, mut world) = group();
        let mut targets = [Target::new(1, &mut main), Target::new(2, &mut world)];
        let answer = session.dispatch_group(
            &mut targets,
            r#"{"id":1,"method":"Runtime.evaluate","params":{"expression":"globalThis.here = 1","contextId":2}}"#,
        );
        assert!(answer[0].contains("\"result\""), "{answer:?}");
        let seen = targets[0].context.eval("globalThis.here === 1").unwrap();
        assert_eq!(
            seen.as_boolean(),
            Some(false),
            "the context the command did not name is untouched"
        );
        let seen = targets[1].context.eval("globalThis.here === 1").unwrap();
        assert_eq!(seen.as_boolean(), Some(true));
    }

    #[test]
    fn a_reset_keeps_the_registry_and_drops_the_client_state() {
        let (mut context, mut session) = session();
        session.add_script(&context, "app.js", SOURCE).unwrap();
        let frames: [ExceptionFrame<'_>; 0] = [];
        session.dispatch(&mut context, r#"{"id":2,"method":"Runtime.enable"}"#);
        assert!(session.exception(&error_report(&frames)).is_some());

        session.reset();
        // A script registered between the two clients is still announced to
        // the second one, and gets its points at that enable rather than now.
        let (_, events) = session.add_script(&context, "other.js", "1;").unwrap();
        assert!(events.is_empty(), "no client is enabled after a reset");
        assert!(
            session.exception(&error_report(&frames)).is_none(),
            "Runtime is off again"
        );
        let answer = session.dispatch(&mut context, r#"{"id":3,"method":"Debugger.enable"}"#);
        assert_eq!(
            answer.len(),
            3,
            "both scripts, then the response: {answer:?}"
        );
        let first: Json = serde_json::from_str(&answer[0]).unwrap();
        assert_eq!(first["method"], "Debugger.scriptParsed");
        assert_eq!(first["params"]["url"], "app.js");
    }

    #[test]
    fn a_step_arms_only_the_halted_context() {
        let (mut session, mut main, mut world) = group();
        let mut targets = [Target::new(1, &mut main), Target::new(2, &mut world)];
        session.dispatch_group(&mut targets, r#"{"id":0,"method":"Debugger.enable"}"#);
        session
            .add_script_in(1, targets[0].context, "app.js", SOURCE)
            .unwrap();
        session.dispatch_group(
            &mut targets,
            r#"{"id":1,"method":"Debugger.setBreakpointByUrl","params":{"url":"app.js","lineNumber":1,"columnNumber":0}}"#,
        );
        assert!(targets[0].context.agent_mut().run_script(SOURCE).is_err());
        assert_eq!(session.poll_group(&mut targets).len(), 1);

        // The step lands on the next statement of the halted activation.
        session.dispatch_group(&mut targets, r#"{"id":2,"method":"Debugger.stepOver"}"#);
        let stepped: Json = serde_json::from_str(&session.poll_group(&mut targets)[0]).unwrap();
        assert_eq!(stepped["params"]["reason"], "step", "{stepped}");
        session.dispatch_group(&mut targets, r#"{"id":3,"method":"Debugger.resume"}"#);

        // The other context was never armed, so its run passes its own points.
        // A different text, or the breakpoint above would match it too: a URL
        // breakpoint is the text's identity, wherever that text runs.
        let other = "var b = 1;\nb = b + 2;\nb;\n";
        assert!(
            targets[1].context.agent_mut().run_script(other).is_ok(),
            "a step in one world must not stop another"
        );
        assert!(session.poll_group(&mut targets).is_empty());
    }

    fn error_report<'a>(frames: &'a [ExceptionFrame<'a>]) -> ExceptionReport<'a> {
        ExceptionReport {
            message: "Uncaught Error: boom",
            url: "app.js",
            line: 2,
            column: 3,
            is_error: true,
            frames,
        }
    }

    #[test]
    fn an_exception_report_names_the_script_and_its_frames() {
        let (mut context, mut session) = session();
        session.add_script(&context, "app.js", SOURCE).unwrap();
        session.dispatch(&mut context, r#"{"id":2,"method":"Runtime.enable"}"#);
        let frames = [
            ExceptionFrame {
                function_name: "inner",
                url: "app.js",
                line: 3,
                column: 5,
            },
            ExceptionFrame {
                function_name: "",
                url: "other.js",
                line: 1,
                column: 1,
            },
        ];
        let (id, events) = session.exception(&error_report(&frames)).expect("enabled");
        assert_eq!(id, 1);
        assert_eq!(events.len(), 1);
        let thrown: Json = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(thrown["method"], "Runtime.exceptionThrown");
        assert!(thrown["params"]["timestamp"].is_number());
        let details = &thrown["params"]["exceptionDetails"];
        assert_eq!(details["exceptionId"], 1);
        assert_eq!(details["text"], "Uncaught Error: boom");
        assert_eq!(details["lineNumber"], 1, "the protocol counts from zero");
        assert_eq!(details["columnNumber"], 2);
        assert_eq!(
            details["scriptId"], "1",
            "the registry's id for the URL: {thrown}"
        );
        assert_eq!(details["exception"]["subtype"], "error");
        let calls = details["stackTrace"]["callFrames"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["functionName"], "inner");
        assert_eq!(calls[0]["scriptId"], "1");
        assert_eq!(calls[0]["lineNumber"], 2);
        assert_eq!(
            calls[1]["scriptId"], "",
            "a frame in a script the session never saw names no id"
        );
    }

    #[test]
    fn an_exception_is_reported_only_to_a_client_that_enabled_runtime() {
        let (_context, mut session) = session();
        let frames: [ExceptionFrame<'_>; 0] = [];
        assert!(
            session.exception(&error_report(&frames)).is_none(),
            "Debugger.enable alone must not start a stream of exceptions"
        );
        assert!(session.revoke_exception(1, "handled").is_empty());
    }

    #[test]
    fn a_revoked_exception_names_the_report_it_withdraws() {
        let (mut context, mut session) = session();
        session.dispatch(&mut context, r#"{"id":2,"method":"Runtime.enable"}"#);
        let frames: [ExceptionFrame<'_>; 0] = [];
        let (id, _) = session.exception(&error_report(&frames)).expect("enabled");
        let events = session.revoke_exception(id, "a handler took it");
        let revoked: Json = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(revoked["method"], "Runtime.exceptionRevoked");
        assert_eq!(revoked["params"]["exceptionId"], id);
        assert_eq!(revoked["params"]["reason"], "a handler took it");
    }
}
