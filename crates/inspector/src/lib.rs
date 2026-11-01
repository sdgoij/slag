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
//! What it does not do yet, because the engine cannot: report more than one call
//! frame (the outermost activation is the only one that stops), expose a scope
//! chain or remote objects (`Runtime.getProperties`), or step. Stepping is the
//! same continuation question as nested stops, one statement at a time.
//!
//! The host drives it as Chrome does: register each script it compiles, feed
//! messages to [`Session::dispatch`], and ask [`Session::poll`] for the events a
//! run produced.

use std::collections::HashMap;

use serde_json::{json, Value as Json};
use slag::{Context, JsValue};

/// One registered script: what the protocol's ids and URLs name.
struct Script {
    id: u32,
    url: String,
    source: String,
    /// The body's debug points as `(start, end)` source offsets, from the
    /// engine. A breakpoint resolves to the first point at or after the
    /// requested position, because the engine matches a point by its own offset
    /// exactly.
    points: Vec<(u32, u32)>,
}

/// A breakpoint the session set. `offset` is the engine's point; `requested` is
/// where the host asked, which is what a `Debugger.paused` frame reports as the
/// resolved location.
struct Breakpoint {
    script_id: u32,
    offset: u32,
}

/// Why a dispatch did not produce a result.
enum DispatchError {
    /// The method exists but the message was unusable.
    Protocol(String),
    /// No such method.
    Unknown,
}

/// A CDP session over one engine context.
pub struct Session {
    scripts: Vec<Script>,
    breakpoints: HashMap<String, Breakpoint>,
    next_breakpoint: u64,
    enabled: bool,
    /// The call frame id of the stop that has been announced and not resumed.
    paused: Option<String>,
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
            paused: None,
        }
    }

    /// Register a script the host is about to run, answering its id and the
    /// events a session that is already enabled owes the client.
    ///
    /// The engine compiles the body here (and caches it), so the points a
    /// breakpoint resolves against are the points the run will carry. A body
    /// that does not compile is the host's compile error, not a debug event.
    pub fn add_script(
        &mut self,
        context: &Context,
        url: &str,
        source: &str,
    ) -> Result<(u32, Vec<String>), String> {
        let points = context.debug_points(source).map_err(|e| e.message)?;
        let id = self.scripts.len() as u32 + 1;
        self.scripts.push(Script {
            id,
            url: url.to_string(),
            source: source.to_string(),
            points,
        });
        let script = self.scripts.last().expect("just pushed");
        let events = if self.enabled {
            vec![script_parsed(script)]
        } else {
            Vec::new()
        };
        Ok((id, events))
    }

    /// Handle one protocol message, answering the response and the events it
    /// produced (events first, as a `paused`/`resumed` pair arrives before the
    /// response to the command that caused it).
    pub fn dispatch(&mut self, context: &mut Context, message: &str) -> Vec<String> {
        let Ok(request) = serde_json::from_str::<Json>(message) else {
            return vec![error(None, -32700, "parse error")];
        };
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Json::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Json::Null);
        match self.handle(context, method, &params) {
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
        let Some(info) = context.paused() else {
            return Vec::new();
        };
        let offset = info.position.map(|span| span.start).unwrap_or(0);
        let (script_id, line, column) = self.location_of(offset).unwrap_or((1, 0, 0));
        let reason = match info.reason {
            slag::PauseReason::Breakpoint => "breakpoint",
            slag::PauseReason::Requested => "other",
        };
        let hit: Vec<String> = self
            .breakpoints
            .iter()
            .filter(|(_, breakpoint)| breakpoint.offset == offset)
            .map(|(id, _)| id.clone())
            .collect();
        let frame = "frame-1";
        self.paused = Some(frame.to_string());
        let mut params = json!({
            "callFrames": [{
                "callFrameId": frame,
                "functionName": "",
                "location": {
                    "scriptId": script_id.to_string(),
                    "lineNumber": line,
                    "columnNumber": column,
                },
                "scopeChain": [],
                "this": {"type": "undefined"},
            }],
            "reason": reason,
        });
        if !hit.is_empty() {
            params["hitBreakpoints"] = json!(hit);
        }
        vec![event("Debugger.paused", params)]
    }

    fn handle(
        &mut self,
        context: &mut Context,
        method: &str,
        params: &Json,
    ) -> Result<(Json, Vec<String>), DispatchError> {
        match method {
            "Runtime.enable" => Ok((json!({}), Vec::new())),
            "Runtime.evaluate" => {
                let expression = params["expression"].as_str().unwrap_or("");
                Ok((evaluation(context.eval(expression)), Vec::new()))
            }
            "Debugger.enable" => {
                context
                    .set_debug_attached(true)
                    .map_err(|error| DispatchError::Protocol(error.message))?;
                self.enabled = true;
                // A client that enables after scripts were registered expects
                // their `scriptParsed` events then.
                let events = self.scripts.iter().map(script_parsed).collect();
                Ok((json!({}), events))
            }
            "Debugger.disable" => {
                let mut events = Vec::new();
                if self.paused.take().is_some() {
                    let _ = context.resume_paused();
                    events.push(event("Debugger.resumed", json!({})));
                }
                let _ = context.set_debug_attached(false);
                self.enabled = false;
                self.breakpoints.clear();
                context.set_breakpoints(&[]);
                Ok((json!({}), events))
            }
            "Debugger.setBreakpointByUrl" => self.set_breakpoint(context, params),
            "Debugger.removeBreakpoint" => {
                let id = params["breakpointId"].as_str().unwrap_or("");
                if self.breakpoints.remove(id).is_none() {
                    return Err(DispatchError::Protocol(format!("no breakpoint {id}")));
                }
                self.push_breakpoints(context);
                Ok((json!({}), Vec::new()))
            }
            "Debugger.pause" => {
                context.request_pause();
                Ok((json!({}), Vec::new()))
            }
            "Debugger.resume" => {
                self.paused = None;
                // Nothing parked is not an error: the client may resume after a
                // stop it already handled.
                let _ = context.resume_paused();
                Ok((json!({}), vec![event("Debugger.resumed", json!({}))]))
            }
            "Debugger.evaluateOnCallFrame" => {
                let expression = params["expression"].as_str().unwrap_or("");
                Ok((evaluation(context.evaluate_paused(expression)), Vec::new()))
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
        context: &mut Context,
        params: &Json,
    ) -> Result<(Json, Vec<String>), DispatchError> {
        let url = params["url"].as_str().unwrap_or("");
        let line = params["lineNumber"].as_u64().unwrap_or(0) as u32;
        let column = params["columnNumber"].as_u64().unwrap_or(0) as u32;
        let Some(script) = self.scripts.iter().find(|script| script.url == url) else {
            return Err(DispatchError::Protocol(format!("no script for {url}")));
        };
        let requested = offset_of(&script.source, line, column);
        let script_id = script.id;
        let resolved = script
            .points
            .iter()
            .map(|(start, _)| *start)
            .find(|start| *start >= requested);
        let source = script.source.clone();
        let id = self.mint_breakpoint();
        let Some(offset) = resolved else {
            // Chrome allows a breakpoint with no location yet: it answers an id
            // and no locations, and the client keeps it.
            return Ok((json!({ "breakpointId": id, "locations": [] }), Vec::new()));
        };
        let (line, column) = position_of(&source, offset);
        self.breakpoints
            .insert(id.clone(), Breakpoint { script_id, offset });
        self.push_breakpoints(context);
        let locations = vec![json!({
            "scriptId": script_id.to_string(),
            "lineNumber": line,
            "columnNumber": column,
        })];
        Ok((
            json!({ "breakpointId": id, "locations": locations }),
            Vec::new(),
        ))
    }

    /// Hand the engine the whole set: it matches a point by offset, so removing
    /// one breakpoint means replacing the list.
    fn push_breakpoints(&self, context: &mut Context) {
        let offsets: Vec<u32> = self.breakpoints.values().map(|bp| bp.offset).collect();
        context.set_breakpoints(&offsets);
    }

    fn mint_breakpoint(&mut self) -> String {
        self.next_breakpoint += 1;
        format!("1:{}:0", self.next_breakpoint)
    }

    /// The script and 0-based position a stop's offset belongs to: the
    /// breakpoint at that offset if there is one, else the first script that
    /// carries a point there.
    fn location_of(&self, offset: u32) -> Option<(u32, u32, u32)> {
        let at_offset = self
            .breakpoints
            .values()
            .find(|breakpoint| breakpoint.offset == offset)
            .map(|breakpoint| breakpoint.script_id);
        if let Some(script_id) = at_offset {
            let script = self.scripts.iter().find(|script| script.id == script_id)?;
            let (line, column) = position_of(&script.source, offset);
            return Some((script.id, line, column));
        }
        let script = self
            .scripts
            .iter()
            .find(|script| script.points.iter().any(|(start, _)| *start == offset))?;
        let (line, column) = position_of(&script.source, offset);
        Some((script.id, line, column))
    }
}

/// A CDP command's answer to an evaluation: a remote object, or the exception
/// details Chrome expects for one that threw.
fn evaluation(result: Result<JsValue, slag::JsError>) -> Json {
    match result {
        Ok(value) => json!({ "result": remote_object(&value) }),
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
fn remote_object(value: &JsValue) -> Json {
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
    json!({ "type": "object" })
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
    fn an_unknown_method_is_refused_in_the_protocol_way() {
        let (mut context, mut session) = session();
        let answer = session.dispatch(&mut context, r#"{"id":7,"method":"Debugger.stepInto"}"#);
        let response: Json = serde_json::from_str(&answer[0]).unwrap();
        assert_eq!(response["error"]["code"], -32601);
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
}
