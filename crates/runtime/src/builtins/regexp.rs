//! The `%RegExp%` intrinsic (spec 22.2): the constructor (RegExpAlloc +
//! RegExpInitialize), the prototype (exec/test/toString, the flag accessors,
//! and the `@@match`/`@@matchAll`/`@@replace`/`@@search`/`@@split` methods),
//! `RegExp.escape`, and `RegExp.prototype[Symbol.toStringTag]`. The compiled
//! pattern lives in the agent's `regexp_data` table; `lastIndex` is an own
//! data property on each instance.

use crux::convert::{to_boolean, to_length, to_number, to_string, to_uint32};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::heap::{GcAny, Trace};
use crux::object::JsObject;
use crux::ops::same_value;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_callable, is_constructor};

use crate::agent::Agent;
use crate::context::{as_object, get_property, get_property_key};
use crate::realm::Realm;

const REGEXP: &str = "%RegExp%";
const REGEXP_PROTO: &str = "%RegExp.prototype%";
const EXEC: &str = "%RegExp.prototype.exec%";
const TEST: &str = "%RegExp.prototype.test%";
const TO_STRING: &str = "%RegExp.prototype.toString%";
const COMPILE: &str = "%RegExp.prototype.compile%";
const GET_SOURCE: &str = "%RegExp.prototype.source%";
const GET_FLAGS: &str = "%RegExp.prototype.flags%";
const GET_GLOBAL: &str = "%RegExp.prototype.global%";
const GET_DOT_ALL: &str = "%RegExp.prototype.dotAll%";
const GET_HAS_INDICES: &str = "%RegExp.prototype.hasIndices%";
const GET_IGNORE_CASE: &str = "%RegExp.prototype.ignoreCase%";
const GET_MULTILINE: &str = "%RegExp.prototype.multiline%";
const GET_STICKY: &str = "%RegExp.prototype.sticky%";
const GET_UNICODE: &str = "%RegExp.prototype.unicode%";
const GET_UNICODE_SETS: &str = "%RegExp.prototype.unicodeSets%";
const MATCH: &str = "%RegExp.prototype[@@match]%";
const MATCH_ALL: &str = "%RegExp.prototype[@@matchAll]%";
const REPLACE: &str = "%RegExp.prototype[@@replace]%";
const SEARCH: &str = "%RegExp.prototype[@@search]%";
const SPLIT: &str = "%RegExp.prototype[@@split]%";
const ESCAPE: &str = "%RegExp.escape%";
const SPECIES: &str = "%get RegExp[Symbol.species]%";
const STRING_ITERATOR: &str = "%RegExpStringIteratorPrototype%";
const STRING_ITERATOR_NEXT: &str = "%RegExpStringIteratorPrototype.next%";

/// The RegExp instance state (the spec's [[OriginalSource]],
/// [[OriginalFlags]], [[RegExpRecord]], [[RegExpMatcher]], and
/// [[RegExpConstructor]]).
#[derive(Debug, Clone)]
pub struct RegExpState {
    pub source: JsString,
    pub flags_text: String,
    pub flags: regexp::Flags,
    pub compiled: regexp::Regex,
    /// The NewTarget that allocated this instance (spec [[RegExpConstructor]]);
    /// RegExp.prototype.compile brand-checks it against %RegExp%.
    pub constructor: Value,
}

impl Trace for RegExpState {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.source.trace(visit);
        self.constructor.trace(visit);
    }
}

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// The RegExp state of an instance; TypeError otherwise.
fn regexp_state(agent: &Agent, value: &Value) -> Result<RegExpState, JsError> {
    match value.kind() {
        ValueKind::Object(obj) => match agent.regexp_data.get(&obj.id()) {
            Some(state) => Ok(state.clone()),
            None => Err(JsError::new(
                ErrorKind::TypeError,
                "Method called on an incompatible receiver".into(),
            )),
        },
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on an incompatible receiver".into(),
        )),
    }
}

/// spec 22.2.4.2 RegExpAlloc: OrdinaryCreateFromConstructor with the four
/// internal slots, plus the `lastIndex` own data property.
fn regexp_alloc(agent: &mut Agent, new_target: &Value) -> Result<Handle<JsObject>, JsError> {
    let proto = get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        *new_target,
    )?;
    let proto = match as_object(&proto) {
        Some(object) => object,
        None => {
            // GetPrototypeFromConstructor fallback (spec 22.2.4.2): the
            // newTarget's realm's %RegExp.prototype%.
            crate::context::get_function_realm(agent, new_target)?
                .intrinsics
                .get("%RegExp.prototype%")
                .and_then(|value| as_object(&value))
                .ok_or_else(|| {
                    JsError::new(
                        ErrorKind::TypeError,
                        "%RegExp.prototype% is not defined".into(),
                    )
                })?
        }
    };
    let object = JsObject::ordinary_object_create(Some(proto));
    object.define_property(
        &JsString::from_utf8("lastIndex"),
        &PropertyDescriptor {
            value: Some(Value::Number(0.0)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    Ok(object)
}

/// spec 22.2.5 RegExpInitialize: validate the flags, parse the pattern, and
/// reset `lastIndex`.
pub fn regexp_initialize(
    agent: &mut Agent,
    object: &Handle<JsObject>,
    pattern: &Value,
    flags: &Value,
    constructor: Value,
) -> Result<Value, JsError> {
    let pattern_text = if matches!(pattern.kind(), ValueKind::Undefined) {
        JsString::from_utf8("")
    } else {
        crate::context::to_string(agent, pattern)?
    };
    let flags_text = if matches!(flags.kind(), ValueKind::Undefined) {
        JsString::from_utf8("")
    } else {
        crate::context::to_string(agent, flags)?
    };
    let parsed_flags = regexp::Flags::parse(flags_text.as_slice())
        .map_err(|e| JsError::new(ErrorKind::SyntaxError, e.message.clone()))?;
    let compiled = regexp::compile(pattern_text.as_slice(), parsed_flags)
        .map_err(|e| JsError::new(ErrorKind::SyntaxError, e.message.clone()))?;
    agent.regexp_data.insert(
        object.id(),
        RegExpState {
            source: pattern_text,
            flags_text: flags_text.to_string_lossy(),
            flags: parsed_flags,
            compiled,
            constructor,
        },
    );
    object.set(&JsString::from_utf8("lastIndex"), Value::Number(0.0), true)?;
    Ok(Value::Object(*object))
}

/// spec 22.2.4.1 RegExp(patternOrRegexp, flags). `new_target` is `undefined`
/// for the call form (dispatch passes the callee's intrinsic for allocation).
fn regexp_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let pattern_or_regexp = args.first().cloned().unwrap_or(Value::Undefined);
    let flags = args.get(1).cloned().unwrap_or(Value::Undefined);
    let pattern_is_regexp = crate::builtins::string::is_regexp(agent, &pattern_or_regexp)?;
    let is_construct = !matches!(new_target.kind(), ValueKind::Undefined);
    let realm = agent.current_realm()?;
    if !is_construct && pattern_is_regexp && matches!(flags.kind(), ValueKind::Undefined) {
        // RegExp(regexp) as a call: the active function is %RegExp%; return
        // the same object when its constructor matches.
        let pattern_ctor = get_property(
            agent,
            &pattern_or_regexp,
            &JsString::from_utf8("constructor"),
            pattern_or_regexp,
        )?;
        let active = realm.intrinsics.get(REGEXP).unwrap_or(Value::Undefined);
        if crux::ops::same_value(&active, &pattern_ctor) {
            return Ok(pattern_or_regexp);
        }
    }
    let effective_new_target = if is_construct {
        *new_target
    } else {
        realm
            .intrinsics
            .get(REGEXP)
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%RegExp% is not defined".into()))?
    };
    let (pattern_source, effective_flags) = if let ValueKind::Object(obj) = pattern_or_regexp.kind()
        && agent.regexp_data.contains_key(&obj.id())
    {
        let state = agent.regexp_data.get(&obj.id()).unwrap().clone();
        let source = Value::String(Handle::new(state.source));
        let flags_value = if matches!(flags.kind(), ValueKind::Undefined) {
            Value::String(Handle::new(JsString::from_utf8(&state.flags_text)))
        } else {
            flags
        };
        (source, flags_value)
    } else if pattern_is_regexp {
        let source = get_property(
            agent,
            &pattern_or_regexp,
            &JsString::from_utf8("source"),
            pattern_or_regexp,
        )?;
        let flags_value = if matches!(flags.kind(), ValueKind::Undefined) {
            get_property(
                agent,
                &pattern_or_regexp,
                &JsString::from_utf8("flags"),
                pattern_or_regexp,
            )?
        } else {
            flags
        };
        (source, flags_value)
    } else {
        (pattern_or_regexp, flags)
    };
    let object = regexp_alloc(agent, &effective_new_target)?;
    regexp_initialize(
        agent,
        &object,
        &pattern_source,
        &effective_flags,
        effective_new_target,
    )
}

/// GetLegacyRegExpStaticProperty (spec B.2.5.2.1): only %RegExp% itself
/// (SameValue) may read the legacy slots; with no match state the value is
/// *undefined*.
fn legacy_static_getter(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let regexp = agent
        .current_realm()?
        .intrinsics
        .get(REGEXP)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%RegExp% is not defined".into()))?;
    if same_value(&regexp, this) {
        Ok(Value::Undefined)
    } else {
        Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp legacy accessor called on a non-%RegExp% receiver".into(),
        ))
    }
}

/// SetLegacyRegExpStaticProperty (spec B.2.5.2.2): the same receiver check
/// as the getter; the slot is not tracked, so a valid set is a no-op.
fn legacy_static_setter(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let regexp = agent
        .current_realm()?
        .intrinsics
        .get(REGEXP)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%RegExp% is not defined".into()))?;
    if same_value(&regexp, this) {
        Ok(Value::Undefined)
    } else {
        Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp legacy accessor called on a non-%RegExp% receiver".into(),
        ))
    }
}

/// Annex B.2.5.1 RegExp.prototype.compile(pattern, flags): recompile this
/// RegExp in place — a RegExp pattern reuses its source and (absent an
/// explicit `flags` argument, which is a TypeError) its flags; otherwise both
/// coerce through ToString. `lastIndex` resets to 0 (immutable-lastindex.js).
pub fn compile(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let ValueKind::Object(obj) = this.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp.prototype.compile requires a RegExp receiver".into(),
        ));
    };
    let current_state = agent.regexp_data.get(&obj.id()).cloned().ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "RegExp.prototype.compile requires a RegExp receiver".into(),
        )
    })?;
    // Annex B.2.5.1 step 4: SameValue(O.[[RegExpConstructor]], %RegExp%) must
    // hold; a subclass instance throws (this-subclass-instance.js).
    let regexp_ctor = agent
        .current_realm()?
        .intrinsics
        .get(REGEXP)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%RegExp% is not defined".into()))?;
    if !crux::ops::same_value(&current_state.constructor, &regexp_ctor) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp.prototype.compile requires a %RegExp% receiver".into(),
        ));
    }
    let pattern = args.first().cloned().unwrap_or(Value::Undefined);
    let flags = args.get(1).cloned().unwrap_or(Value::Undefined);
    let (pattern_source, effective_flags) = if let ValueKind::Object(pat) = pattern.kind()
        && agent.regexp_data.contains_key(&pat.id())
    {
        if !matches!(flags.kind(), ValueKind::Undefined) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "cannot supply flags when compiling from another RegExp".into(),
            ));
        }
        let state = agent.regexp_data.get(&pat.id()).unwrap().clone();
        (
            Value::String(Handle::new(state.source)),
            Value::String(Handle::new(JsString::from_utf8(&state.flags_text))),
        )
    } else {
        (pattern, flags)
    };
    regexp_initialize(
        agent,
        &obj,
        &pattern_source,
        &effective_flags,
        current_state.constructor,
    )?;
    Ok(*this)
}

/// spec 22.2.2.2 RegExpBuiltinExec: the lastIndex protocol and the match
/// array. Returns `Ok(None)` for a failed match (null).
fn regexp_builtin_exec(
    agent: &mut Agent,
    regexp: &Value,
    string: &JsString,
) -> Result<Option<Value>, JsError> {
    let ValueKind::Object(obj) = regexp.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp exec called on an incompatible receiver".into(),
        ));
    };
    let Some(state) = agent.regexp_data.get(&obj.id()).cloned() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp exec called on an incompatible receiver".into(),
        ));
    };
    let length = string.len();
    let last_index_value = get_property(
        agent,
        regexp,
        &JsString::from_utf8("lastIndex"),
        *regexp,
    )?;
    let mut last_index = to_length(to_number(&last_index_value)?) as usize;
    let global = state.flags.g;
    let sticky = state.flags.y;
    let has_indices = state.flags.d;
    if !global && !sticky {
        last_index = 0;
    }
    let match_result = loop {
        if last_index > length {
            if global || sticky {
                obj.set(&JsString::from_utf8("lastIndex"), Value::Number(0.0), true)?;
            }
            return Ok(None);
        }
        if let Some(m) = state.compiled.exec_at(string.as_slice(), last_index) {
            break m;
        }
        if sticky {
            obj.set(&JsString::from_utf8("lastIndex"), Value::Number(0.0), true)?;
            return Ok(None);
        }
        last_index = state
            .compiled
            .advance_string_index(string.as_slice(), last_index);
    };
    let result = match_result;
    let end_index = result[0].map(|(_, e)| e).unwrap_or(last_index);
    if global || sticky {
        obj.set(
            &JsString::from_utf8("lastIndex"),
            Value::Number(end_index as f64),
            true,
        )?;
    }
    let capturing_groups = state.compiled.capturing_groups;
    let array = crate::builtins::array::array_create(agent, (capturing_groups + 1) as f64)?;
    array.create_data_property(
        &JsString::from_utf8("index"),
        Value::Number(last_index as f64),
    )?;
    array.create_data_property(
        &JsString::from_utf8("input"),
        Value::String(Handle::new(string.clone())),
    )?;
    // The `groups` object (only when the pattern has any GroupName). Keys
    // appear in source order; a duplicate name reports the last of its
    // groups that matched.
    let groups = if state.compiled.has_group_names {
        let groups_obj = JsObject::ordinary_object_create(None);
        let named = groups_obj;
        for name in &state.compiled.named_group_order {
            let value = match last_named_span(&state.compiled, &result, name) {
                Some((s, e)) => Value::String(Handle::new(substring(string, s, e))),
                None => Value::Undefined,
            };
            named.create_data_property(&JsString::from_utf16(&to_utf16(name)), value)?;
        }
        Value::Object(groups_obj)
    } else {
        Value::Undefined
    };
    array.create_data_property(&JsString::from_utf8("groups"), groups)?;
    // Group 0 and each capture.
    for (i, capture) in result.iter().enumerate() {
        let value = match capture {
            Some((s, e)) => Value::String(Handle::new(substring(string, *s, *e))),
            None => Value::Undefined,
        };
        array.create_data_property(&JsString::from_utf8(&i.to_string()), value)?;
    }
    // `d` flag: the indices array with named groups (spec MakeIndicesArray).
    if has_indices {
        let indices_obj =
            crate::builtins::array::array_create(agent, (capturing_groups + 1) as f64)?;
        for (i, capture) in result.iter().enumerate() {
            let pair = match capture {
                Some((s, e)) => pair_array(agent, *s, *e)?,
                None => Value::Undefined,
            };
            indices_obj.create_data_property(&JsString::from_utf8(&i.to_string()), pair)?;
        }
        let groups_indices = if state.compiled.has_group_names {
            let groups_indices = JsObject::ordinary_object_create(None);
            for name in &state.compiled.named_group_order {
                let pair = match last_named_span(&state.compiled, &result, name) {
                    Some((s, e)) => pair_array(agent, s, e)?,
                    None => Value::Undefined,
                };
                groups_indices
                    .create_data_property(&JsString::from_utf16(&to_utf16(name)), pair)?;
            }
            Value::Object(groups_indices)
        } else {
            Value::Undefined
        };
        indices_obj.create_data_property(&JsString::from_utf8("groups"), groups_indices)?;
        array.create_data_property(&JsString::from_utf8("indices"), Value::Object(indices_obj))?;
    }
    Ok(Some(Value::Object(array)))
}

fn pair_array(agent: &mut Agent, start: usize, end: usize) -> Result<Value, JsError> {
    let pair = crate::builtins::array::array_create(agent, 2.0)?;
    pair.create_data_property(
        &JsString::from_utf8("0"),
        Value::Number(if start == usize::MAX {
            f64::NAN
        } else {
            start as f64
        }),
    )?;
    pair.create_data_property(
        &JsString::from_utf8("1"),
        Value::Number(if end == usize::MAX {
            f64::NAN
        } else {
            end as f64
        }),
    )?;
    Ok(Value::Object(pair))
}

/// The span of the last of `name`'s groups that matched, for the `groups`
/// object (spec: a duplicate name reports the last matching group).
fn last_named_span(
    compiled: &regexp::Regex,
    result: &[Option<(usize, usize)>],
    name: &[u32],
) -> Option<(usize, usize)> {
    compiled
        .named_groups
        .get(name)?
        .iter()
        .rev()
        .find_map(|&index| result[index])
}

fn substring(s: &JsString, from: usize, to: usize) -> JsString {
    let len = s.len();
    let from = from.min(len);
    let to = to.min(len).max(from);
    JsString::from_utf16(&s.as_slice()[from..to])
}

fn to_utf16(cps: &[u32]) -> Vec<u16> {
    let mut out = Vec::new();
    for &cp in cps {
        if cp <= 0xFFFF {
            out.push(cp as u16);
        } else {
            let x = cp - 0x10000;
            out.push(0xD800 + (x >> 10) as u16);
            out.push(0xDC00 + (x & 0x3FF) as u16);
        }
    }
    out
}

/// spec 22.2.5.1 RegExp.prototype.exec(string).
fn exec(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let string =
        crate::context::to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    match regexp_builtin_exec(agent, this, &string)? {
        Some(array) => Ok(array),
        None => Ok(Value::Null),
    }
}

/// spec 22.2.5.2 RegExp.prototype.test(string).
fn test(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let string =
        crate::context::to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let matched = regexp_builtin_exec(agent, this, &string)?.is_some();
    Ok(Value::Boolean(matched))
}

/// spec 22.2.5.10 RegExp.prototype.toString: compose from Get(R, "source")
/// and Get(R, "flags") so overridden accessors are honored and the flags
/// come back in the canonical (sorted) order.
fn to_string_method(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    regexp_state(agent, this)?; // thisRegExpValue brand check
    let source = get_property(agent, this, &JsString::from_utf8("source"), *this)?;
    let flags = get_property(agent, this, &JsString::from_utf8("flags"), *this)?;
    let source_text = crate::context::to_string(agent, &source)?;
    let flags_text = crate::context::to_string(agent, &flags)?;
    let mut units = Vec::with_capacity(source_text.len() + flags_text.len() + 2);
    units.push(b'/' as u16);
    units.extend_from_slice(source_text.as_slice());
    units.push(b'/' as u16);
    units.extend_from_slice(flags_text.as_slice());
    Ok(Value::String(Handle::new(JsString::from_utf16(&units))))
}

/// EscapeRegExpPattern (spec 22.2.4.4): escape `/` and line terminators so
/// the literal round-trips; empty patterns render as `(?:)`. A slash that is
/// already part of an escape sequence is left alone.
/// The source getter's output: the original pattern text, with a literal
/// `/` outside an escape escaped, and lone surrogates preserved as raw code
/// units (a UTF-8 round-trip would replace them with U+FFFD).
fn escape_source(source: &JsString) -> Vec<u16> {
    if source.is_empty() {
        return "(?:)".encode_utf16().collect();
    }
    let units = source.as_slice();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        let escaped = i > 0 && units[i - 1] == b'\\' as u16;
        match u {
            0x2F if !escaped => out.extend_from_slice(&[b'\\' as u16, b'/' as u16]),
            0x0A => out.extend_from_slice(&[b'\\' as u16, b'n' as u16]),
            0x0D => out.extend_from_slice(&[b'\\' as u16, b'r' as u16]),
            0x2028 => out.extend_from_slice(&"\\u2028".encode_utf16().collect::<Vec<u16>>()),
            0x2029 => out.extend_from_slice(&"\\u2029".encode_utf16().collect::<Vec<u16>>()),
            _ => out.push(u),
        }
        i += 1;
    }
    out
}

/// The flag getters (spec 22.2.5.3-22.2.5.9, 22.2.5.14-15).
fn get_flag(agent: &mut Agent, this: &Value, name: &str) -> Result<Value, JsError> {
    // spec: a receiver that is %RegExp.prototype% itself (no [[OriginalFlags]]
    // slot) returns undefined; any other object without the slot throws.
    if agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get(REGEXP_PROTO))
        .is_some_and(|proto| crux::ops::same_value(&proto, this))
    {
        return Ok(Value::Undefined);
    }
    let state = regexp_state(agent, this)?;
    let value = match name {
        "global" => state.flags.g,
        "dotAll" => state.flags.s,
        "hasIndices" => state.flags.d,
        "ignoreCase" => state.flags.i,
        "multiline" => state.flags.m,
        "sticky" => state.flags.y,
        "unicode" => state.flags.u,
        "unicodeSets" => state.flags.v,
        _ => return Ok(Value::Undefined),
    };
    Ok(Value::Boolean(value))
}

/// spec 22.2.5.14 RegExp.prototype.flags.
fn get_flags(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    // spec 22.2.4.2: the flags getter composes by reading each flag property
    // via Get, in the spec order (hasIndices, global, ignoreCase, multiline,
    // dotAll, unicode, unicodeSets, sticky), so an overridden accessor or
    // own data property is honored.
    if !matches!(this.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp.prototype.flags called on a non-object".into(),
        ));
    }
    let mut text = String::new();
    for (flag, name) in [
        ('d', "hasIndices"),
        ('g', "global"),
        ('i', "ignoreCase"),
        ('m', "multiline"),
        ('s', "dotAll"),
        ('u', "unicode"),
        ('v', "unicodeSets"),
        ('y', "sticky"),
    ] {
        let value = match get_property(agent, this, &JsString::from_utf8(name), *this) {
            Ok(value) => value,
            // The built-in flag accessors reject receivers without
            // [[RegExpMatcher]]; %RegExp.prototype% and similar objects
            // report every flag as false (the flags getter returns "").
            Err(e)
                if e.kind == ErrorKind::TypeError
                    && e.message.contains("incompatible receiver") =>
            {
                Value::Undefined
            }
            Err(e) => return Err(e),
        };
        if to_boolean(&value) {
            text.push(flag);
        }
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

/// spec 22.2.5.13 RegExp.prototype.source.
fn get_source(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    // spec: %RegExp.prototype% itself reports "(?:)".
    if agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get(REGEXP_PROTO))
        .is_some_and(|proto| crux::ops::same_value(&proto, this))
    {
        return Ok(Value::String(Handle::new(JsString::from_utf8("(?:)"))));
    }
    let state = regexp_state(agent, this)?;
    let text = escape_source(&state.source);
    Ok(Value::String(Handle::new(JsString::from_utf16(&text))))
}

/// spec 22.2.7.1 RegExp.prototype[@@match](string).
fn symbol_match(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    if !matches!(this.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp.prototype[Symbol.match] called on non-object".into(),
        ));
    }
    let string =
        crate::context::to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    // spec 22.2.7.4 steps 3-5: the flags come from Get(rx, "flags") (whose
    // accessor composes global/unicode/etc.); a non-global result returns
    // RegExpExec directly.
    let flags_value = get_property(agent, this, &JsString::from_utf8("flags"), *this)?;
    let flags = crate::context::to_string(agent, &flags_value)?;
    let global = flags.as_slice().contains(&(b'g' as u16));
    if !global {
        return match regexp_exec(agent, this, &string)? {
            Some(array) => Ok(array),
            None => Ok(Value::Null),
        };
    }
    let full_unicode =
        flags.as_slice().contains(&(b'u' as u16)) || flags.as_slice().contains(&(b'v' as u16));
    let this_obj = as_object(this).unwrap();
    this_obj.set(&JsString::from_utf8("lastIndex"), Value::Number(0.0), true)?;
    // GC-2: the freshly-boxed match strings accumulate in a local Vec the
    // stack scan cannot see while the next `regexp_exec`/`Handle::new`
    // allocates — suppress `--gc-stress` for the loop so the half-built Vec
    // cannot be swept (the elements land on the traced result array after).
    let _stress = crate::ir::StressSuppress::new();
    let mut result: Vec<Value> = Vec::new();
    loop {
        match regexp_exec(agent, this, &string)? {
            None => break,
            Some(array) => {
                let matched_value =
                    get_property(agent, &array, &JsString::from_utf8("0"), array)?;
                let matched = crate::context::to_string(agent, &matched_value)?;
                result.push(Value::String(Handle::new(matched.clone())));
                if matched.is_empty() {
                    let this_index = to_length(to_number(&get_property(
                        agent,
                        this,
                        &JsString::from_utf8("lastIndex"),
                        *this,
                    )?)?) as usize;
                    let next_index = simple_advance(&string, this_index, full_unicode);
                    this_obj.set(
                        &JsString::from_utf8("lastIndex"),
                        Value::Number(next_index as f64),
                        true,
                    )?;
                }
            }
        }
    }
    if result.is_empty() {
        return Ok(Value::Null);
    }
    let array = crate::builtins::array::array_create(agent, result.len() as f64)?;
    for (i, value) in result.into_iter().enumerate() {
        array.create_data_property(&JsString::from_utf8(&i.to_string()), value)?;
    }
    Ok(Value::Object(array))
}

/// spec 22.2.7.6 RegExp.prototype[@@matchAll](string).
fn symbol_match_all(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    if !matches!(this.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp.prototype[Symbol.matchAll] called on non-object".into(),
        ));
    }
    let string =
        crate::context::to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    // spec 22.2.7.6 steps 4-6: the matcher is a clone built through
    // SpeciesConstructor with the Get(rx, "flags") string; global/fullUnicode
    // for the iterator come from the flags string, never from the matcher's
    // own accessors.
    let realm = agent.current_realm()?;
    let default_ctor = realm
        .intrinsics
        .get(REGEXP)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%RegExp% is not defined".into()))?;
    let ctor = species_constructor(agent, this, default_ctor)?;
    let flags_value = get_property(agent, this, &JsString::from_utf8("flags"), *this)?;
    let flags = crate::context::to_string(agent, &flags_value)?;
    let global = flags.as_slice().contains(&(b'g' as u16));
    let full_unicode =
        flags.as_slice().contains(&(b'u' as u16)) || flags.as_slice().contains(&(b'v' as u16));
    let matcher = crate::function::construct(
        agent,
        &ctor,
        &[*this, Value::String(Handle::new(flags))],
        &ctor,
    )?;
    // The pinned fixtures keep the 2018 MatchAllIterator step: the initial
    // lastIndex is read and length-coerced here (a throwing valueOf
    // propagates from matchAll itself) and cached onto the matcher, so later
    // writes to the original regexp's lastIndex do not move the iterator
    // (`this-lastindex-cached.js`).
    let last_index = get_property(agent, this, &JsString::from_utf8("lastIndex"), *this)?;
    let last_index = to_length(to_number(&last_index)?);
    as_object(&matcher)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "matcher is not an object".into()))?
        .set(
            &JsString::from_utf8("lastIndex"),
            Value::Number(last_index as f64),
            true,
        )?;
    let proto = realm
        .intrinsics
        .get(STRING_ITERATOR)
        .and_then(|value| as_object(&value));
    let iterator = JsObject::ordinary_object_create(proto);
    agent.regexp_string_iter_data.insert(
        iterator.id(),
        (matcher, string, global, full_unicode, false),
    );
    Ok(Value::Object(iterator))
}

/// spec 22.2.7.4 RegExp.prototype[@@search](string).
fn symbol_search(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    if !matches!(this.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp.prototype[Symbol.search] called on non-object".into(),
        ));
    }
    let string =
        crate::context::to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    // spec 22.2.7.4: preserve and restore lastIndex around the exec; the
    // restore is skipped when the exec itself throws (match-err fixtures).
    let previous = get_property(agent, this, &JsString::from_utf8("lastIndex"), *this)?;
    let this_obj = as_object(this).unwrap();
    if !crux::ops::same_value(&previous, &Value::Number(0.0)) {
        this_obj.set(&JsString::from_utf8("lastIndex"), Value::Number(0.0), true)?;
    }
    let result = regexp_exec(agent, this, &string)?;
    let current = get_property(agent, this, &JsString::from_utf8("lastIndex"), *this)?;
    if !crux::ops::same_value(&current, &previous) {
        this_obj.set(&JsString::from_utf8("lastIndex"), previous, true)?;
    }
    match result {
        None => Ok(Value::Number(-1.0)),
        Some(array) => {
            let index = get_property(agent, &array, &JsString::from_utf8("index"), array)?;
            Ok(Value::Number(to_integer_or_infinity(to_number(&index)?)))
        }
    }
}

/// spec 22.2.7.5 RegExp.prototype[@@split](string, limit).
fn symbol_split(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    if !matches!(this.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp.prototype[Symbol.split] called on non-object".into(),
        ));
    }
    let string =
        crate::context::to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    // spec 22.2.7.15 steps 4-10: the splitter is a sticky clone built through
    // SpeciesConstructor; the flags come from Get(rx, "flags") so a custom
    // flags property or getter is honored. The splitter must be constructed
    // before the limit is coerced: a side-effectful ToUint32 (e.g. a
    // valueOf that recompiles rx) must not change the splitter's pattern
    // (toint32-limit-recompiles-source.js).
    let realm = agent.current_realm()?;
    let default_ctor = realm
        .intrinsics
        .get(REGEXP)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%RegExp% is not defined".into()))?;
    let ctor = species_constructor(agent, this, default_ctor)?;
    let flags_value = get_property(agent, this, &JsString::from_utf8("flags"), *this)?;
    let flags = crate::context::to_string(agent, &flags_value)?;
    let unicode_matching =
        flags.as_slice().contains(&(b'u' as u16)) || flags.as_slice().contains(&(b'v' as u16));
    let mut new_flags = flags.as_slice().to_vec();
    if !new_flags.contains(&(b'y' as u16)) {
        new_flags.push(b'y' as u16);
    }
    let splitter = crate::function::construct(
        agent,
        &ctor,
        &[
            *this,
            Value::String(Handle::new(JsString::from_utf16(&new_flags))),
        ],
        &ctor,
    )?;
    let limit = args.get(1).cloned().unwrap_or(Value::Undefined);
    let lim = if matches!(limit.kind(), ValueKind::Undefined) {
        u32::MAX
    } else {
        to_uint32(to_number(&limit)?)
    };
    let mut array: Vec<Value> = Vec::new();
    // GC-2: the split segments/captures accumulate in a local Vec the stack
    // scan cannot see while the next exec/substring boxes — suppress
    // `--gc-stress` for the loop so the half-built Vec cannot be swept.
    let _stress = crate::ir::StressSuppress::new();
    let size = string.len();
    if lim == 0 {
        return array_from_values(agent, &[]);
    }
    if string.is_empty() {
        let match_result = regexp_exec(agent, &splitter, &string)?;
        if match_result.is_some() {
            return array_from_values(agent, &[]);
        }
        return array_from_values(agent, &[Value::String(Handle::new(string))]);
    }
    let splitter_state = regexp_state(agent, &splitter).ok();
    let mut last_match_end = 0usize;
    let mut search_index = last_match_end;
    while search_index < size {
        if let Some(splitter_obj) = as_object(&splitter) {
            splitter_obj.set(
                &JsString::from_utf8("lastIndex"),
                Value::Number(search_index as f64),
                true,
            )?;
        }
        match regexp_exec(agent, &splitter, &string)? {
            None => {
                search_index = match &splitter_state {
                    Some(state) => state
                        .compiled
                        .advance_string_index(string.as_slice(), search_index),
                    None => simple_advance(&string, search_index, unicode_matching),
                };
            }
            Some(match_result) => {
                let match_end_value = get_property(
                    agent,
                    &splitter,
                    &JsString::from_utf8("lastIndex"),
                    splitter,
                )?;
                let match_end = (to_length(to_number(&match_end_value)?) as usize).min(size);
                if match_end == last_match_end {
                    search_index = match &splitter_state {
                        Some(state) => state
                            .compiled
                            .advance_string_index(string.as_slice(), search_index),
                        None => simple_advance(&string, search_index, unicode_matching),
                    };
                } else {
                    let segment = substring(&string, last_match_end, search_index);
                    array.push(Value::String(Handle::new(segment)));
                    if array.len() == lim as usize {
                        return array_from_values(agent, &array);
                    }
                    last_match_end = match_end;
                    let result_length = array_length(agent, &match_result)?;
                    let captures_count = result_length.saturating_sub(1);
                    for i in 1..=captures_count {
                        let capture = get_property(
                            agent,
                            &match_result,
                            &JsString::from_utf8(&i.to_string()),
                            match_result,
                        )?;
                        array.push(capture);
                        if array.len() == lim as usize {
                            return array_from_values(agent, &array);
                        }
                    }
                    search_index = last_match_end;
                }
            }
        }
    }
    let tail = substring(&string, last_match_end, size);
    array.push(Value::String(Handle::new(tail)));
    array_from_values(agent, &array)
}

/// RegExpExec (spec 22.2.6.2.1): call the `exec` method when present, else
/// RegExpBuiltinExec.
fn regexp_exec(agent: &mut Agent, rx: &Value, string: &JsString) -> Result<Option<Value>, JsError> {
    let exec = get_property(agent, rx, &JsString::from_utf8("exec"), *rx)?;
    if is_callable(&exec) {
        let result = crate::function::call(
            agent,
            &exec,
            *rx,
            &[Value::String(Handle::new(string.clone()))],
        )?;
        return match result.kind() {
            ValueKind::Null => Ok(None),
            ValueKind::Object(_) | ValueKind::Function(_) => Ok(Some(result)),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "RegExp exec must return an object or null".into(),
            )),
        };
    }
    regexp_builtin_exec(agent, rx, string)
}

/// SpeciesConstructor (spec 7.3.24): the exemplar's `constructor`, then its
/// `[Symbol.species]` (a null/undefined species means the constructor
/// itself).
fn species_constructor(
    agent: &mut Agent,
    exemplar: &Value,
    default_ctor: Value,
) -> Result<Value, JsError> {
    let ctor = get_property(
        agent,
        exemplar,
        &JsString::from_utf8("constructor"),
        *exemplar,
    )?;
    if matches!(ctor.kind(), ValueKind::Undefined) {
        return Ok(default_ctor);
    }
    if !is_constructor(&ctor) && !is_callable(&ctor) && !matches!(ctor.kind(), ValueKind::Object(_))
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "constructor is not an object".into(),
        ));
    }
    let species_key = PropertyKey::Symbol(crux::symbol::well_known("species"));
    let species = get_property_key(agent, &ctor, &species_key, ctor)?;
    match species.kind() {
        // The pinned fixtures follow the ES6 text: a null/undefined species
        // falls back to the default constructor (an inherited `constructor`
        // like %Object% must not become the splitter's constructor).
        ValueKind::Null | ValueKind::Undefined => Ok(default_ctor),
        _ if is_constructor(&species) => Ok(species),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "species is not a constructor".into(),
        )),
    }
}

/// AdvanceStringIndex (spec 22.2.6.2.5) for a splitter without a compiled
/// regex (a custom species may return an arbitrary object).
fn simple_advance(string: &JsString, index: usize, unicode: bool) -> usize {
    if index >= string.len() {
        return index + 1;
    }
    let unit = string.as_slice()[index];
    if unicode
        && (0xD800..=0xDBFF).contains(&unit)
        && let Some(&next) = string.as_slice().get(index + 1)
        && (0xDC00..=0xDFFF).contains(&next)
    {
        return index + 2;
    }
    index + 1
}

fn to_integer_or_infinity(n: f64) -> f64 {
    if n.is_nan() || n == 0.0 {
        0.0
    } else if n.is_infinite() {
        n
    } else {
        n.trunc()
    }
}

fn array_from_values(agent: &Agent, values: &[Value]) -> Result<Value, JsError> {
    crate::builtins::array::array_from_values(agent, values)
}

/// spec 22.2.7.3 RegExp.prototype[@@replace](string, replaceValue).
fn symbol_replace(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    if !matches!(this.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp.prototype[Symbol.replace] called on non-object".into(),
        ));
    }
    let string =
        crate::context::to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let replace_value = args.get(1).cloned().unwrap_or(Value::Undefined);
    let functional = is_callable(&replace_value);
    let replace_text = if functional {
        None
    } else {
        Some(crate::context::to_string(agent, &replace_value)?)
    };
    // spec 22.2.7.3 steps 6-11: the flags come from Get(rx, "flags"); a
    // global rx is reset to lastIndex 0 before the exec loop.
    let flags_value = get_property(agent, this, &JsString::from_utf8("flags"), *this)?;
    let flags = crate::context::to_string(agent, &flags_value)?;
    let global = flags.as_slice().contains(&(b'g' as u16));
    let full_unicode =
        flags.as_slice().contains(&(b'u' as u16)) || flags.as_slice().contains(&(b'v' as u16));
    let string_length = string.len();
    if global {
        let obj = as_object(this).unwrap();
        obj.set(&JsString::from_utf8("lastIndex"), Value::Number(0.0), true)?;
    }
    // GC-2: the exec-result arrays accumulate in a local Vec (and the
    // functional replacer's argument Vec is built per match) in heap
    // buffers the stack scan cannot see while the next exec/replacer call
    // allocates — suppress `--gc-stress` for the whole replace so those
    // buffers cannot be swept.
    let _stress = crate::ir::StressSuppress::new();
    let mut results: Vec<Value> = Vec::new();
    loop {
        match regexp_exec(agent, this, &string)? {
            None => break,
            Some(array) => {
                results.push(array);
                if !global {
                    break;
                }
                let match_value =
                    get_property(agent, &array, &JsString::from_utf8("0"), array)?;
                let match_string = crate::context::to_string(agent, &match_value)?;
                if match_string.is_empty() {
                    let this_index = to_length(to_number(&get_property(
                        agent,
                        this,
                        &JsString::from_utf8("lastIndex"),
                        *this,
                    )?)?) as usize;
                    let next_index = simple_advance(&string, this_index, full_unicode);
                    let obj = as_object(this).unwrap();
                    obj.set(
                        &JsString::from_utf8("lastIndex"),
                        Value::Number(next_index as f64),
                        true,
                    )?;
                }
            }
        }
    }
    let mut accumulated: Vec<u16> = Vec::new();
    let mut next_source_position = 0usize;
    for result in results {
        let matched_value =
            get_property(agent, &result, &JsString::from_utf8("0"), result)?;
        let matched = crate::context::to_string(agent, &matched_value)?;
        let match_length = matched.len();
        let position_value = to_number(&get_property(
            agent,
            &result,
            &JsString::from_utf8("index"),
            result,
        )?)?;
        let position = to_integer_or_infinity(position_value);
        let position = if position.is_finite() {
            (position as usize).clamp(0, string_length)
        } else if position < 0.0 {
            0
        } else {
            string_length
        };
        let mut captures: Vec<Value> = Vec::new();
        let result_length = array_length(agent, &result)?;
        let captures_count = result_length.saturating_sub(1);
        for i in 1..=captures_count {
            let capture = get_property(
                agent,
                &result,
                &JsString::from_utf8(&i.to_string()),
                result,
            )?;
            captures.push(capture);
        }
        let named_captures = get_property(
            agent,
            &result,
            &JsString::from_utf8("groups"),
            result,
        )?;
        let replacement_string = if functional {
            let mut replacer_args = vec![Value::String(Handle::new(matched.clone()))];
            replacer_args.extend(captures);
            replacer_args.push(Value::Number(position as f64));
            replacer_args.push(Value::String(Handle::new(string.clone())));
            if !matches!(named_captures.kind(), ValueKind::Undefined) {
                replacer_args.push(named_captures);
            }
            let called =
                crate::function::call(agent, &replace_value, Value::Undefined, &replacer_args)?;
            crate::context::to_string(agent, &called)?
        } else {
            let named = if matches!(named_captures.kind(), ValueKind::Undefined) {
                None
            } else {
                Some(crate::context::to_object(agent, &named_captures)?)
            };
            let text = match replace_text.as_ref() {
                Some(text) => text,
                None => &JsString::from_utf8(""),
            };
            crate::builtins::string::get_substitution_public(
                agent, &matched, &string, position, &captures, named, text,
            )?
        };
        if position >= next_source_position {
            accumulated
                .extend_from_slice(substring(&string, next_source_position, position).as_slice());
            accumulated.extend_from_slice(replacement_string.as_slice());
            next_source_position = position + match_length;
        }
    }
    if next_source_position < string_length {
        accumulated
            .extend_from_slice(substring(&string, next_source_position, string_length).as_slice());
    }
    Ok(Value::String(Handle::new(JsString::from_utf16(
        &accumulated,
    ))))
}

fn array_length(agent: &mut Agent, value: &Value) -> Result<usize, JsError> {
    let length = get_property(agent, value, &JsString::from_utf8("length"), *value)?;
    Ok(to_length(to_number(&length)?) as usize)
}

/// spec 22.2.6.2.1 %RegExpStringIteratorPrototype%.next.
fn string_iterator_next(
    agent: &mut Agent,
    this: &Value,
    _args: &[Value],
) -> Result<Value, JsError> {
    let ValueKind::Object(obj) = this.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "%RegExpStringIteratorPrototype%.next called on an incompatible receiver".into(),
        ));
    };
    let Some(entry) = agent.regexp_string_iter_data.get(&obj.id()).cloned() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "%RegExpStringIteratorPrototype%.next called on an incompatible receiver".into(),
        ));
    };
    let (regexp, string, global, full_unicode, done) = entry;
    if done {
        return iter_result(Value::Undefined, true);
    }
    if !global && let Some(e) = agent.regexp_string_iter_data.get_mut(&obj.id()) {
        e.4 = true;
    }
    // spec 22.2.8.2 steps 8-10: RegExpExec (the custom exec method wins), and
    // a global iterator with an empty match advances lastIndex past it.
    let result = regexp_exec(agent, &regexp, &string)?;
    match result {
        Some(array) => {
            if global {
                let matched_value =
                    get_property(agent, &array, &JsString::from_utf8("0"), array)?;
                let matched = crate::context::to_string(agent, &matched_value)?;
                if matched.is_empty() {
                    let this_index = to_length(to_number(&get_property(
                        agent,
                        &regexp,
                        &JsString::from_utf8("lastIndex"),
                        regexp,
                    )?)?) as usize;
                    let next_index = simple_advance(&string, this_index, full_unicode);
                    if let Some(rx_obj) = as_object(&regexp) {
                        rx_obj.set(
                            &JsString::from_utf8("lastIndex"),
                            Value::Number(next_index as f64),
                            true,
                        )?;
                    }
                }
            }
            iter_result(array, false)
        }
        None => {
            if let Some(e) = agent.regexp_string_iter_data.get_mut(&obj.id()) {
                e.4 = true;
            }
            iter_result(Value::Undefined, true)
        }
    }
}

/// CreateIterResultObject (spec 7.4.9).
fn iter_result(value: Value, done: bool) -> Result<Value, JsError> {
    let object = JsObject::ordinary_object_create(None);
    object.create_data_property(&JsString::from_utf8("value"), value)?;
    object.create_data_property(&JsString::from_utf8("done"), Value::Boolean(done))?;
    Ok(Value::Object(object))
}

/// spec 22.2.4.3 RegExp.escape(string).
fn escape(_agent: &mut Agent, _this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if !matches!(value.kind(), ValueKind::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "RegExp.escape expects a string".into(),
        ));
    }
    let text = to_string(&value)?;
    let escaped = regexp::escape::escape(text.as_slice());
    Ok(Value::String(Handle::new(JsString::from_utf8(&escaped))))
}

/// Install the RegExp intrinsics and the global `RegExp` binding.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let regexp_proto = JsObject::ordinary_object_create(object_proto);
    let regexp_proto_value = Value::Object(regexp_proto);

    let regexp_ctor = Function::create_builtin(
        Some(JsString::from_utf8("RegExp")),
        2,
        placeholder("RegExp"),
        Some(Box::new(placeholder("RegExp"))),
        None,
    )?;
    let regexp_ctor_value = Value::Function(regexp_ctor);

    realm.intrinsics.define(REGEXP, regexp_ctor_value);
    realm
        .intrinsics
        .define(REGEXP_PROTO, regexp_proto_value);

    regexp_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(regexp_proto_value),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    regexp_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(regexp_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // spec 22.2.4.5: get RegExp [ @@species ] — an accessor on the
    // constructor returning `this` (the split/matchAll cloning consults it).
    let species_func = Function::create_builtin(
        Some(JsString::from_utf8("get [Symbol.species]")),
        0,
        placeholder("species"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(SPECIES, Value::Function(species_func));
    regexp_ctor.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("species")),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(species_func)),
            set: Some(Value::Undefined),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // RegExp.escape (spec 22.2.4.3).
    let escape_func = Function::create_builtin(
        Some(JsString::from_utf8("escape")),
        1,
        placeholder("escape"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(ESCAPE, Value::Function(escape_func));
    regexp_ctor.define_property(
        &JsString::from_utf8("escape"),
        &PropertyDescriptor {
            value: Some(Value::Function(escape_func)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // The prototype methods.
    let methods: [(&str, &str, u64); 4] = [
        ("exec", EXEC, 1),
        ("test", TEST, 1),
        ("toString", TO_STRING, 0),
        ("compile", COMPILE, 2),
    ];
    for (name, key, length) in methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(func));
        regexp_proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Function(func)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    // The flag accessors (data-property getters).
    let accessors: [(&str, &str); 10] = [
        ("source", GET_SOURCE),
        ("flags", GET_FLAGS),
        ("global", GET_GLOBAL),
        ("dotAll", GET_DOT_ALL),
        ("hasIndices", GET_HAS_INDICES),
        ("ignoreCase", GET_IGNORE_CASE),
        ("multiline", GET_MULTILINE),
        ("sticky", GET_STICKY),
        ("unicode", GET_UNICODE),
        ("unicodeSets", GET_UNICODE_SETS),
    ];
    for (name, key) in accessors {
        let getter = Function::create_builtin(
            Some(JsString::from_utf8(&format!("get {name}"))),
            0,
            placeholder("get"),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(getter));
        regexp_proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: None,
                writable: None,
                get: Some(Value::Function(getter)),
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    // Annex B.2.5.2: the legacy RegExp static accessors. Each named accessor
    // shares its getter (and input's setter) with a `$`-alias; only `input`
    // has a setter (SetLegacyRegExpStaticProperty). GetLegacyRegExpStatic-
    // Property throws unless the receiver is %RegExp% itself; with no match
    // state the slots read as undefined (the annexB fixtures only assert the
    // descriptors and the receiver TypeError).
    let install_legacy = |realm: &Handle<Realm>,
                          ctor: &Handle<Function>,
                          name: &str,
                          alias: &str,
                          with_setter: bool|
     -> Result<(), JsError> {
        let getter = Function::create_builtin(
            Some(JsString::from_utf8(&format!("get {name}"))),
            0,
            placeholder("legacy"),
            None,
            None,
        )?;
        realm
            .intrinsics
            .define(&format!("%RegExp.legacy.{name}%"), Value::Function(getter));
        let setter = if with_setter {
            let setter = Function::create_builtin(
                Some(JsString::from_utf8(&format!("set {name}"))),
                1,
                placeholder("legacy"),
                None,
                None,
            )?;
            realm.intrinsics.define(
                &format!("%RegExp.legacy.{name}.set%"),
                Value::Function(setter),
            );
            Some(setter)
        } else {
            None
        };
        let mut properties = vec![name];
        if !alias.is_empty() {
            properties.push(alias);
        }
        for property in properties {
            ctor.define_property(
                &JsString::from_utf8(property),
                &PropertyDescriptor {
                    value: None,
                    writable: None,
                    get: Some(Value::Function(getter)),
                    set: setter.map(Value::Function).or(Some(Value::Undefined)),
                    enumerable: Some(false),
                    configurable: Some(true),
                },
            )?;
        }
        Ok(())
    };
    install_legacy(realm, &regexp_ctor, "input", "$_", true)?;
    install_legacy(realm, &regexp_ctor, "lastMatch", "$&", false)?;
    install_legacy(realm, &regexp_ctor, "lastParen", "$+", false)?;
    install_legacy(realm, &regexp_ctor, "leftContext", "$\u{60}", false)?;
    install_legacy(realm, &regexp_ctor, "rightContext", "$'", false)?;
    for index in 1..=9u32 {
        install_legacy(realm, &regexp_ctor, &format!("${index}"), "", false)?;
    }

    // The symbol methods.
    let symbol_methods: [(&str, &str, u64); 5] = [
        ("@@match", MATCH, 1),
        ("@@matchAll", MATCH_ALL, 1),
        ("@@replace", REPLACE, 2),
        ("@@search", SEARCH, 1),
        ("@@split", SPLIT, 2),
    ];
    for (symbol_name, key, length) in symbol_methods {
        let name = format!("[Symbol.{}]", symbol_name.trim_start_matches("@@"));
        let func = Function::create_builtin(
            Some(JsString::from_utf8(&name)),
            length,
            placeholder("symbol"),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(func));
        let symbol_key = PropertyKey::Symbol(
            crux::symbol::well_known(symbol_name.trim_start_matches("@@"))
        );
        regexp_proto.define_property_key(
            &symbol_key,
            &PropertyDescriptor {
                value: Some(Value::Function(func)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    // The `@@toStringTag` of RegExp.prototype was removed from the spec
    // (RegExp is branded via [[RegExpMatcher]] in Object.prototype.toString).

    // %RegExpStringIteratorPrototype% (spec 22.2.6.2).
    let iterator_proto = JsObject::ordinary_object_create(object_proto);
    let iterator_proto_value = Value::Object(iterator_proto);
    realm
        .intrinsics
        .define(STRING_ITERATOR, iterator_proto_value);
    let next_func = Function::create_builtin(
        Some(JsString::from_utf8("next")),
        0,
        placeholder("next"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(STRING_ITERATOR_NEXT, Value::Function(next_func));
    iterator_proto.define_property(
        &JsString::from_utf8("next"),
        &PropertyDescriptor {
            value: Some(Value::Function(next_func)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    iterator_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag")),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "RegExp String Iterator",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("RegExp"),
        &PropertyDescriptor {
            value: Some(regexp_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The RegExp members that need the agent, dispatched by intrinsic identity.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    // RegExp(...) called as a function: NewTarget is undefined; the
    // constructor resolves %RegExp% for allocation.
    if intrinsics.get(REGEXP).as_ref() == Some(callee) {
        return Some(regexp_construct(agent, args, &Value::Undefined));
    }
    if intrinsics.get(EXEC).as_ref() == Some(callee) {
        return Some(exec(agent, this, args));
    }
    if intrinsics.get(TEST).as_ref() == Some(callee) {
        return Some(test(agent, this, args));
    }
    if intrinsics.get(TO_STRING).as_ref() == Some(callee) {
        return Some(to_string_method(agent, this, args));
    }
    if intrinsics.get(COMPILE).as_ref() == Some(callee) {
        return Some(compile(agent, this, args));
    }
    for key in [
        "%RegExp.legacy.input%",
        "%RegExp.legacy.lastMatch%",
        "%RegExp.legacy.lastParen%",
        "%RegExp.legacy.leftContext%",
        "%RegExp.legacy.rightContext%",
        "%RegExp.legacy.$1%",
        "%RegExp.legacy.$2%",
        "%RegExp.legacy.$3%",
        "%RegExp.legacy.$4%",
        "%RegExp.legacy.$5%",
        "%RegExp.legacy.$6%",
        "%RegExp.legacy.$7%",
        "%RegExp.legacy.$8%",
        "%RegExp.legacy.$9%",
    ] {
        if intrinsics.get(key).as_ref() == Some(callee) {
            return Some(legacy_static_getter(agent, this));
        }
    }
    if intrinsics.get("%RegExp.legacy.input.set%").as_ref() == Some(callee) {
        return Some(legacy_static_setter(agent, this));
    }
    if intrinsics.get(GET_SOURCE).as_ref() == Some(callee) {
        return Some(get_source(agent, this));
    }
    if intrinsics.get(GET_FLAGS).as_ref() == Some(callee) {
        return Some(get_flags(agent, this));
    }
    for (key, name) in [
        (GET_GLOBAL, "global"),
        (GET_DOT_ALL, "dotAll"),
        (GET_HAS_INDICES, "hasIndices"),
        (GET_IGNORE_CASE, "ignoreCase"),
        (GET_MULTILINE, "multiline"),
        (GET_STICKY, "sticky"),
        (GET_UNICODE, "unicode"),
        (GET_UNICODE_SETS, "unicodeSets"),
    ] {
        if intrinsics.get(key).as_ref() == Some(callee) {
            return Some(get_flag(agent, this, name));
        }
    }
    if intrinsics.get(MATCH).as_ref() == Some(callee) {
        return Some(symbol_match(agent, this, args));
    }
    if intrinsics.get(MATCH_ALL).as_ref() == Some(callee) {
        return Some(symbol_match_all(agent, this, args));
    }
    if intrinsics.get(REPLACE).as_ref() == Some(callee) {
        return Some(symbol_replace(agent, this, args));
    }
    if intrinsics.get(SEARCH).as_ref() == Some(callee) {
        return Some(symbol_search(agent, this, args));
    }
    if intrinsics.get(SPLIT).as_ref() == Some(callee) {
        return Some(symbol_split(agent, this, args));
    }
    if intrinsics.get(ESCAPE).as_ref() == Some(callee) {
        return Some(escape(agent, this, args));
    }
    if intrinsics.get(SPECIES).as_ref() == Some(callee) {
        return Some(Ok(*this));
    }
    if intrinsics.get(STRING_ITERATOR_NEXT).as_ref() == Some(callee) {
        return Some(string_iterator_next(agent, this, args));
    }
    None
}

pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(REGEXP).as_ref() == Some(callee) {
        return Some(regexp_construct(agent, args, new_target));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    fn run(source: &str) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm()?;
        agent.run_script(source)
    }

    fn text(source: &str) -> String {
        match run(source).unwrap().kind() {
            ValueKind::String(s) => s.to_string_lossy(),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    fn number(source: &str) -> f64 {
        match run(source).unwrap().kind() {
            ValueKind::Number(n) => n,
            other => panic!("expected a number, got {other:?}"),
        }
    }

    fn bool(source: &str) -> bool {
        match run(source).unwrap().kind() {
            ValueKind::Boolean(b) => b,
            other => panic!("expected a boolean, got {other:?}"),
        }
    }

    #[test]
    fn split_result_uses_the_array_prototype() {
        // The @@split result array must carry %Array.prototype% (it was
        // created with a null prototype, so `.join` failed).
        assert_eq!(text("('ab').split(/(.)/).join('|')"), "|a||b|");
        assert_eq!(text("('a1b2').split(/\\d/).join(',')"), "a,b,");
        assert!(bool("('a,b'.split(',') instanceof Array)"));
    }

    #[test]
    fn constructor_and_literals() {
        assert_eq!(run("typeof RegExp").unwrap().to_string(), "function");
        assert!(bool("/ab+c/.test('abbbc')"));
        assert!(!bool("/ab+c/.test('ac')"));
        assert!(bool("/abc/i.test('ABC')"));
        assert!(bool("new RegExp('a+b').test('aaab')"));
        assert!(bool("RegExp('x').test('x')"));
        // RegExp(regexp) returns the same object.
        assert_eq!(
            run("let r = /a/g; RegExp(r) === r").unwrap(),
            Value::Boolean(true)
        );
        assert!(matches!(
            run("new RegExp('(')"),
            Err(e) if e.kind == ErrorKind::SyntaxError
        ));
        assert!(matches!(
            run("new RegExp('a', 'x')"),
            Err(e) if e.kind == ErrorKind::SyntaxError
        ));
        assert!(matches!(
            run("new RegExp('a', 'gg')"),
            Err(e) if e.kind == ErrorKind::SyntaxError
        ));
    }

    #[test]
    fn exec_captures_and_groups() {
        assert_eq!(text("/(\\d+)-(\\d+)/.exec('x 12-34 y')[2]"), "34");
        assert_eq!(number("/(\\d+)-(\\d+)/.exec('x 12-34 y').index"), 2.0);
        assert_eq!(text("/(\\d+)-(\\d+)/.exec('x 12-34 y').input"), "x 12-34 y");
        assert_eq!(text("/(?<y>\\d+)/.exec('x42').groups.y"), "42");
        assert_eq!(
            run("/(a)?b/.exec('b')[1]").unwrap().to_string(),
            "undefined"
        );
        // lastIndex protocol with g.
        assert_eq!(number("var r = /a/g; r.exec('aba'); r.lastIndex"), 1.0);
        assert_eq!(
            number("var r = /a/g; r.exec('aba'); r.exec('aba'); r.lastIndex"),
            3.0
        );
        assert_eq!(
            run("var r = /a/g; r.exec('aba'); r.exec('aba'); r.exec('aba')")
                .unwrap()
                .to_string(),
            "null"
        );
    }

    #[test]
    fn sticky_and_global() {
        assert!(bool("var r = /a/y; r.lastIndex = 1; r.test('ba')"));
        assert!(!bool("var r = /a/y; r.lastIndex = 0; r.test('ba')"));
        assert_eq!(number("'a1b2c3'.match(/[0-9]/g).length"), 3.0);
        assert_eq!(text("'a1b2c3'.match(/[0-9]/g)[1]"), "2");
        assert_eq!(text("'aaa'.match(/a*?/g).length === 4 ? 'ok' : 'no'"), "ok");
    }

    #[test]
    fn replace_search_split() {
        assert_eq!(text("'hello world'.replace(/o/g, '0')"), "hell0 w0rld");
        assert_eq!(text("'hello'.replace(/l/g, 'L')"), "heLLo");
        assert_eq!(text("'abc'.replace(/(b)/, '[$1]')"), "a[b]c");
        assert_eq!(text("'aXbXc'.split(/X/)[1]"), "b");
        assert_eq!(number("'abab'.split(/b/).length"), 3.0);
        assert_eq!(number("'abc'.search(/b/)"), 1.0);
        assert_eq!(number("'abc'.search(/x/)"), -1.0);
        assert_eq!(text("'a1b2'.replace(/(\\d)/g, '<$1>')"), "a<1>b<2>");
    }

    #[test]
    fn match_all_iterator() {
        assert_eq!(number("('a1b2'[Symbol.iterator] ? 1 : 0)"), 1.0);
        assert_eq!(
            text("var it = 'a1b2'.matchAll(/\\d/g); it.next().value[0]"),
            "1"
        );
        assert_eq!(
            text("var it = 'a1b2'.matchAll(/\\d/g); it.next().value[0]; it.next().value[0]"),
            "2"
        );
        assert_eq!(
            text(
                "var it = 'ab'.matchAll(/\\d/g); it.next().done === false; it.next().done === true ? 'y' : 'n'"
            ),
            "y"
        );
    }

    #[test]
    fn prototype_surface() {
        assert_eq!(text("/foo/.source"), "foo");
        assert_eq!(text("/foo/gi.flags"), "gi");
        assert!(bool("/a/g.global"));
        assert!(bool("/a/i.ignoreCase"));
        assert!(bool("/a/m.multiline"));
        assert!(bool("/a/s.dotAll"));
        assert!(bool("/a/y.sticky"));
        assert!(bool("/a/u.unicode"));
        assert!(bool("/a/v.unicodeSets"));
        assert!(bool("/a/d.hasIndices"));
        assert_eq!(text("/a/.toString()"), "/a/");
        assert_eq!(text(r"/a\/b/.toString()"), "/a\\/b/");
        assert_eq!(text("new RegExp('').toString()"), "/(?:)/");
        assert_eq!(text("/()/.toString()"), "/()/");
        assert_eq!(
            text("Object.prototype.toString.call(/a/)"),
            "[object RegExp]"
        );
        assert_eq!(number("RegExp.prototype.exec.length"), 1.0);
    }

    #[test]
    fn escape_and_d_indices() {
        assert_eq!(text("RegExp.escape('a.b')"), "\\x61\\.b");
        assert_eq!(text("RegExp.escape('1')"), "\\x31");
        assert!(matches!(
            run("RegExp.escape(5)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert_eq!(
            text(
                "/(a)(b)/d.exec('ab').indices[1][0] === 0 && /(a)(b)/d.exec('ab').indices[2][1] === 2 ? 'y' : 'n'"
            ),
            "y"
        );
    }

    #[test]
    fn unicode_and_backrefs() {
        assert!(bool("/\\u{1F600}/u.test('\\u{1F600}')"));
        assert!(bool("/(a|b)\\1/.test('aa')"));
        assert!(!bool("/(a|b)\\1/.test('ab')"));
        assert!(bool("/(?<w>ab)\\k<w>/.test('abab')"));
        assert!(!bool("/(?<w>ab)\\k<w>/.test('abcd')"));
        assert_eq!(text("/((a)|(ab))((c)|(bc))/.exec('abc')[0]"), "abc");
        assert_eq!(
            run("/((a)|(ab))((c)|(bc))/.exec('abc')[3]")
                .unwrap()
                .to_string(),
            "undefined"
        );
        assert!(bool("/(?<=a)b/.test('ab')"));
        assert!(!bool("/(?<!a)b/.test('ab')"));
        assert_eq!(text("'aaa'.match(/a+/g).length === 1 ? 'y' : 'n'"), "y");
    }
}
