//! The `%String%` intrinsic (spec 22.1): the constructor, the statics
//! (`String.raw`, `fromCharCode`, `fromCodePoint`), and `%String.prototype%`
//! (the full prototype-method surface incl. the Annex B HTML wrappers and the
//! String iterator). Methods dispatch by intrinsic identity (the %eval%
//! pattern) because ToString on object receivers and the `@@match`/`@@split`/
//! `@@replace`/`@@search` delegations need the agent; `String.raw` reads
//! array-likes through the agent too.

use crux::convert::{to_integer_or_infinity, to_length, to_number, to_string, to_uint32};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_callable};

use crate::agent::Agent;
use crate::context::{as_object, get_property, get_property_key, to_object};
use crate::realm::Realm;

const STRING: &str = "%String%";
const STRING_PROTO: &str = "%String.prototype%";
const RAW: &str = "%String.raw%";
const AT: &str = "%String.prototype.at%";
const CHAR_AT: &str = "%String.prototype.charAt%";
const CHAR_CODE_AT: &str = "%String.prototype.charCodeAt%";
const CODE_POINT_AT: &str = "%String.prototype.codePointAt%";
const CONCAT: &str = "%String.prototype.concat%";
const ENDS_WITH: &str = "%String.prototype.endsWith%";
const INCLUDES: &str = "%String.prototype.includes%";
const INDEX_OF: &str = "%String.prototype.indexOf%";
const IS_WELL_FORMED: &str = "%String.prototype.isWellFormed%";
const LAST_INDEX_OF: &str = "%String.prototype.lastIndexOf%";
const LOCALE_COMPARE: &str = "%String.prototype.localeCompare%";
const MATCH: &str = "%String.prototype.match%";
const MATCH_ALL: &str = "%String.prototype.matchAll%";
const NORMALIZE: &str = "%String.prototype.normalize%";
const PAD_END: &str = "%String.prototype.padEnd%";
const PAD_START: &str = "%String.prototype.padStart%";
const REPEAT: &str = "%String.prototype.repeat%";
const REPLACE: &str = "%String.prototype.replace%";
const REPLACE_ALL: &str = "%String.prototype.replaceAll%";
const SEARCH: &str = "%String.prototype.search%";
const SLICE: &str = "%String.prototype.slice%";
const SPLIT: &str = "%String.prototype.split%";
const STARTS_WITH: &str = "%String.prototype.startsWith%";
const SUBSTR: &str = "%String.prototype.substr%";
const SUBSTRING: &str = "%String.prototype.substring%";
const TO_LOCALE_LOWER_CASE: &str = "%String.prototype.toLocaleLowerCase%";
const TO_LOCALE_UPPER_CASE: &str = "%String.prototype.toLocaleUpperCase%";
const TO_LOWER_CASE: &str = "%String.prototype.toLowerCase%";
const TO_UPPER_CASE: &str = "%String.prototype.toUpperCase%";
const TO_STRING: &str = "%String.prototype.toString%";
const TO_WELL_FORMED: &str = "%String.prototype.toWellFormed%";
const TRIM: &str = "%String.prototype.trim%";
const TRIM_END: &str = "%String.prototype.trimEnd%";
const TRIM_START: &str = "%String.prototype.trimStart%";
const VALUE_OF: &str = "%String.prototype.valueOf%";
const ITERATOR: &str = "%String.prototype[@@iterator]%";
const STRING_ITERATOR: &str = "%StringIteratorPrototype%";
const STRING_ITERATOR_NEXT: &str = "%StringIteratorPrototype.next%";

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// spec 7.2.1 RequireObjectCoercible: the generic String methods reject
/// `undefined`/`null` receivers but accept any other value.
fn require_object_coercible(value: &Value) -> Result<(), JsError> {
    if matches!(value.kind(), ValueKind::Undefined | ValueKind::Null) {
        Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert undefined or null to object".into(),
        ))
    } else {
        Ok(())
    }
}

/// spec 22.1.3.1 ThisStringValue: a String or a String wrapper object (the
/// String exotic carries `[[StringData]]` in its kind).
fn this_string_value(this: &Value) -> Result<JsString, JsError> {
    match this.kind() {
        ValueKind::String(s) => Ok(s.as_ref().clone()),
        ValueKind::Object(obj) => match &obj.kind {
            crux::object::ObjectKind::String(s) => Ok(s.as_ref().clone()),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "String.prototype method called on an incompatible receiver".into(),
            )),
        },
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "String.prototype method called on an incompatible receiver".into(),
        )),
    }
}

/// The "substring of `s` from `from` to `to`" helper (spec §substring),
/// clamping both ends into range.
fn substring(s: &JsString, from: usize, to: usize) -> JsString {
    let len = s.len();
    let from = from.min(len);
    let to = to.min(len).max(from);
    JsString::from_utf16(&s.as_slice()[from..to])
}

/// spec 22.1.3.8.1 StringIndexOf.
fn string_index_of(s: &JsString, search: &JsString, from: usize) -> Option<usize> {
    let len = s.len();
    if search.is_empty() && from <= len {
        return Some(from);
    }
    let search_len = search.len();
    if from + search_len > len {
        return None;
    }
    let units = s.as_slice();
    let needle = search.as_slice();
    for i in from..=len - search_len {
        if &units[i..i + search_len] == needle {
            return Some(i);
        }
    }
    None
}

/// spec 22.1.3.9.1 StringLastIndexOf (the caller guarantees
/// `from + searchLength ≤ length`).
fn string_last_index_of(s: &JsString, search: &JsString, from: usize) -> Option<usize> {
    let search_len = search.len();
    let units = s.as_slice();
    let needle = search.as_slice();
    for i in (0..=from).rev() {
        if i + search_len <= units.len() && &units[i..i + search_len] == needle {
            return Some(i);
        }
    }
    None
}

/// spec 7.3.13 ToClampedIndex: relative negative indices, clamped to
/// `[0, length]`.
fn to_clamped_index(agent: &mut Agent, value: &Value, len: usize) -> Result<usize, JsError> {
    let int = to_integer_or_infinity(crate::context::to_number(agent, value)?);
    let index = if int.is_finite() && int < 0.0 {
        len as f64 + int
    } else {
        int
    };
    Ok(index.clamp(0.0, len as f64) as usize)
}

/// A pure String static or non-agent method: `(this, args) -> value`.
type StringFn = fn(&Value, &[Value]) -> Result<Value, JsError>;

/// String(value) / new String(value) (spec 22.1.1.1): ToString the argument;
/// only the call form returns SymbolDescriptiveString for Symbols — the
/// constructor ToStrings, and ToString of a Symbol throws.
fn string_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let proto = instance_proto(agent, new_target)?;
    let text = match args.first() {
        // spec 22.1.1.1 step 2: SymbolDescriptiveString applies only when
        // NewTarget is undefined (new String(Symbol) throws, symbol-wrapping).
        Some(value) if matches!(value.kind(), ValueKind::Symbol(_)) => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Cannot convert a Symbol value to a string".into(),
            ));
        }
        Some(value) => crate::context::to_string(agent, value)?,
        None => JsString::from_utf8(""),
    };
    let object = JsObject::string_create(text, proto)?;
    Ok(Value::Object(object))
}

/// GetPrototypeFromConstructor (spec 10.1.14) for the String wrapper.
fn instance_proto(
    agent: &mut Agent,
    new_target: &Value,
) -> Result<Option<Handle<JsObject>>, JsError> {
    let proto = get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        new_target.clone(),
    )?;
    match as_object(&proto) {
        Some(object) => Ok(Some(object)),
        None => {
            // GetPrototypeFromConstructor fallback (spec 10.1.14): the
            // newTarget's realm's %String.prototype%.
            Ok(crate::context::get_function_realm(agent, new_target)?
                .intrinsics
                .get("%String.prototype%")
                .and_then(|value| as_object(&value)))
        }
    }
}

fn string_call(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let text = match args.first() {
        Some(value) => match value.kind() {
            ValueKind::Symbol(symbol) => {
                JsString::from_utf8(&crux::symbol::descriptive_string(&symbol))
            }
            _ => crate::context::to_string(agent, value)?,
        },
        None => JsString::from_utf8(""),
    };
    Ok(Value::String(Handle::new(text)))
}

/// spec 22.1.2.2 String.fromCharCode: each argument ToUint16 → code unit.
fn from_char_code(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let mut units = Vec::with_capacity(args.len());
    for arg in args {
        units.push((to_uint32(to_number(arg)?) & 0xFFFF) as u16);
    }
    Ok(Value::String(Handle::new(JsString::from_utf16(&units))))
}

/// spec 22.1.2.3 String.fromCodePoint: integral code points in range.
fn from_code_point(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let mut code_points = Vec::with_capacity(args.len());
    for arg in args {
        let cp = to_number(arg)?;
        if cp.trunc() != cp || cp < 0.0 || cp > 0x10FFFF as f64 {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "Invalid code point".into(),
            ));
        }
        code_points.push(cp as u32);
    }
    let text = crux::string::code_points_to_string(&code_points)?;
    Ok(Value::String(Handle::new(text)))
}

/// spec 22.1.2.4 String.raw: the raw template strings interleaved with the
/// substitutions, driven through array-like property reads.
fn raw(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let template = args.first().cloned().unwrap_or(Value::Undefined);
    let substitutions = &args[1..];
    let cooked = to_object(agent, &template)?;
    let raw_value = get_property(agent, &cooked, &JsString::from_utf8("raw"), cooked.clone())?;
    let literals = to_object(agent, &raw_value)?;
    let literal_count = length_of_array_like(agent, &literals)?;
    if literal_count == 0 {
        return Ok(Value::String(Handle::new(JsString::from_utf8(""))));
    }
    let mut result: Vec<u16> = Vec::new();
    let mut next_index = 0u64;
    loop {
        let literal_value = get_property(
            agent,
            &literals,
            &JsString::from_utf8(&next_index.to_string()),
            literals.clone(),
        )?;
        result.extend_from_slice(crate::context::to_string(agent, &literal_value)?.as_slice());
        if next_index + 1 == literal_count {
            return Ok(Value::String(Handle::new(JsString::from_utf16(&result))));
        }
        if let Some(sub) = substitutions.get(next_index as usize) {
            result.extend_from_slice(crate::context::to_string(agent, sub)?.as_slice());
        }
        next_index += 1;
    }
}

/// LengthOfArrayLike (spec 7.3.22).
fn length_of_array_like(agent: &mut Agent, value: &Value) -> Result<u64, JsError> {
    let length = get_property(agent, value, &JsString::from_utf8("length"), value.clone())?;
    Ok(to_length(crate::context::to_number(agent, &length)?))
}

/// spec 22.1.3.2 String.prototype.at.
fn at(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let len = s.len();
    let index = args.first().cloned().unwrap_or(Value::Undefined);
    let int = to_integer_or_infinity(crate::context::to_number(agent, &index)?);
    let k = if int.is_finite() && int < 0.0 {
        len as f64 + int
    } else {
        int
    };
    if !k.is_finite() || k < 0.0 || k >= len as f64 {
        return Ok(Value::Undefined);
    }
    let k = k as usize;
    Ok(Value::String(Handle::new(substring(&s, k, k + 1))))
}

/// spec 22.1.3.3 String.prototype.charAt.
fn char_at(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let position = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let size = s.len() as f64;
    if position < 0.0 || position >= size {
        return Ok(Value::String(Handle::new(JsString::from_utf8(""))));
    }
    let p = position as usize;
    Ok(Value::String(Handle::new(substring(&s, p, p + 1))))
}

/// spec 22.1.3.4 String.prototype.charCodeAt.
fn char_code_at(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let position = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let size = s.len() as f64;
    if position < 0.0 || position >= size {
        return Ok(Value::Number(f64::NAN));
    }
    Ok(Value::Number(
        s.code_unit(position as usize).unwrap_or(0) as f64
    ))
}

/// spec 22.1.3.5 String.prototype.codePointAt.
fn code_point_at(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let position = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let size = s.len() as f64;
    if position < 0.0 || position >= size {
        return Ok(Value::Undefined);
    }
    let (cp, _, _) = s.code_point_at(position as usize).unwrap_or((0, false, 1));
    Ok(Value::Number(cp as f64))
}

/// spec 22.1.3.6 String.prototype.concat.
fn concat(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let mut units: Vec<u16> = crate::context::to_string(agent, this)?.as_slice().to_vec();
    for arg in args {
        units.extend_from_slice(crate::context::to_string(agent, arg)?.as_slice());
    }
    Ok(Value::String(Handle::new(JsString::from_utf16(&units))))
}

/// spec 22.1.3.7 String.prototype.endsWith.
fn ends_with(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let search_value = args.first().cloned().unwrap_or(Value::Undefined);
    if is_regexp(agent, &search_value)? {
        return Err(regexp_argument_error("endsWith"));
    }
    let search = crate::context::to_string(agent, &search_value)?;
    let len = s.len();
    let end = match args.get(1).cloned().unwrap_or(Value::Undefined) {
        v if v.is_undefined() => len,
        other => to_integer_or_infinity(crate::context::to_number(agent, &other)?)
            .clamp(0.0, len as f64) as usize,
    };
    let search_len = search.len();
    if search_len == 0 {
        return Ok(Value::Boolean(true));
    }
    if end < search_len {
        return Ok(Value::Boolean(false));
    }
    Ok(Value::Boolean(
        substring(&s, end - search_len, end) == search,
    ))
}

/// spec 22.1.3.8 String.prototype.includes.
fn includes(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let search_value = args.first().cloned().unwrap_or(Value::Undefined);
    if is_regexp(agent, &search_value)? {
        return Err(regexp_argument_error("includes"));
    }
    let search = crate::context::to_string(agent, &search_value)?;
    let len = s.len();
    let position = args.get(1).cloned().unwrap_or(Value::Undefined);
    let start = to_integer_or_infinity(crate::context::to_number(agent, &position)?)
        .clamp(0.0, len as f64) as usize;
    Ok(Value::Boolean(
        string_index_of(&s, &search, start).is_some(),
    ))
}

/// spec 22.1.3.9 String.prototype.indexOf.
fn index_of(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let search =
        crate::context::to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let len = s.len();
    let position = args.get(1).cloned().unwrap_or(Value::Undefined);
    let start = to_integer_or_infinity(crate::context::to_number(agent, &position)?)
        .clamp(0.0, len as f64) as usize;
    match string_index_of(&s, &search, start) {
        Some(i) => Ok(Value::Number(i as f64)),
        None => Ok(Value::Number(-1.0)),
    }
}

/// spec 22.1.3.10 String.prototype.isWellFormed.
fn is_well_formed(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    Ok(Value::Boolean(is_well_formed_string(&s)))
}

fn is_well_formed_string(s: &JsString) -> bool {
    let units = s.as_slice();
    let mut i = 0;
    while i < units.len() {
        let unit = units[i];
        if (0xD800..=0xDBFF).contains(&unit) {
            if let Some(&low) = units.get(i + 1)
                && (0xDC00..=0xDFFF).contains(&low)
            {
                i += 2;
                continue;
            }
            return false;
        }
        if (0xDC00..=0xDFFF).contains(&unit) {
            return false;
        }
        i += 1;
    }
    true
}

/// spec 22.1.3.11 String.prototype.lastIndexOf.
fn last_index_of(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let search =
        crate::context::to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let len = s.len();
    let search_len = search.len();
    let Some(max_start) = len.checked_sub(search_len) else {
        return Ok(Value::Number(-1.0));
    };
    let number_position =
        crate::context::to_number(agent, &args.get(1).cloned().unwrap_or(Value::Undefined))?;
    let start = if number_position.is_nan() {
        max_start
    } else {
        to_integer_or_infinity(number_position).clamp(0.0, max_start as f64) as usize
    };
    match string_last_index_of(&s, &search, start) {
        Some(i) => Ok(Value::Number(i as f64)),
        None => Ok(Value::Number(-1.0)),
    }
}

/// spec 22.1.3.19 String.prototype.localeCompare: an implementation-defined
/// ordering; this engine compares code units lexicographically.
fn locale_compare(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let that =
        crate::context::to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    // Canonically equivalent strings must compare equal (Unicode default
    // collation); normalize both sides to NFC first.
    let a = normalize_text(&s);
    let b = normalize_text(&that);
    Ok(Value::Number(match a.as_slice().cmp(b.as_slice()) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    }))
}

fn normalize_text(text: &JsString) -> JsString {
    let cps: Vec<u32> = text.code_points().collect();
    let normalized = unicode::normalize_code_points(&cps, unicode::NormalizationForm::Nfc);
    crux::string::code_points_to_string(&normalized).unwrap_or_else(|_| text.clone())
}

/// spec 22.1.3.12 String.prototype.match: `@@match` delegation, then
/// RegExpCreate (Phase 11 provides `%RegExp%`).
fn match_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let regexp = args.first().cloned().unwrap_or(Value::Undefined);
    // spec 22.1.3.17 step 2: a non-Object regexp argument never consults
    // its @@match property; it is coerced by RegExpCreate instead.
    if matches!(regexp.kind(), ValueKind::Object(_) | ValueKind::Function(_))
        && let Some(matcher) = crate::expr::get_method(agent, &regexp, "@@match")?
    {
        return crate::function::call(agent, &matcher, regexp, std::slice::from_ref(this));
    }
    let s = crate::context::to_string(agent, this)?;
    let rx = regexp_create(agent, &regexp, None)?;
    let matcher = crate::expr::get_method(agent, &rx, "@@match")?
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "RegExp has no @@match".into()))?;
    crate::function::call(agent, &matcher, rx, &[Value::String(Handle::new(s))])
}

/// spec 22.1.3.13 String.prototype.matchAll: `@@matchAll` delegation with the
/// global-flag check, then RegExpCreate with `"g"` (Phase 11).
fn match_all(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let regexp = args.first().cloned().unwrap_or(Value::Undefined);
    // spec 22.1.3.13 step 2: a non-Object regexp argument never consults
    // its @@matchAll property; it is coerced by RegExpCreate instead.
    if matches!(regexp.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        if is_regexp(agent, &regexp)? {
            let flags = get_property(
                agent,
                &regexp,
                &JsString::from_utf8("flags"),
                regexp.clone(),
            )?;
            require_object_coercible(&flags)?;
            let flag_text = crate::context::to_string(agent, &flags)?;
            if !flag_text.as_slice().contains(&(b'g' as u16)) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "String.prototype.matchAll called with a non-global RegExp".into(),
                ));
            }
        }
        if let Some(matcher) = crate::expr::get_method(agent, &regexp, "@@matchAll")? {
            return crate::function::call(agent, &matcher, regexp, std::slice::from_ref(this));
        }
    }
    let s = crate::context::to_string(agent, this)?;
    let rx = regexp_create(agent, &regexp, Some("g"))?;
    let matcher = crate::expr::get_method(agent, &rx, "@@matchAll")?
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "RegExp has no @@matchAll".into()))?;
    crate::function::call(agent, &matcher, rx, &[Value::String(Handle::new(s))])
}

/// spec 22.1.3.17 String.prototype.normalize.
fn normalize(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let form = args.first().cloned().unwrap_or(Value::Undefined);
    let form = if matches!(form.kind(), ValueKind::Undefined) {
        unicode::NormalizationForm::Nfc
    } else {
        let form_text = crate::context::to_string(agent, &form)?;
        if eq_ascii(&form_text, "NFC") {
            unicode::NormalizationForm::Nfc
        } else if eq_ascii(&form_text, "NFD") {
            unicode::NormalizationForm::Nfd
        } else if eq_ascii(&form_text, "NFKC") {
            unicode::NormalizationForm::Nfkc
        } else if eq_ascii(&form_text, "NFKD") {
            unicode::NormalizationForm::Nfkd
        } else {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "The normalization form should be one of NFC, NFD, NFKC, NFKD".into(),
            ));
        }
    };
    let code_points: Vec<u32> = s.code_points().collect();
    let normalized = unicode::normalize_code_points(&code_points, form);
    let text = crux::string::code_points_to_string(&normalized)?;
    Ok(Value::String(Handle::new(text)))
}

/// spec 22.1.3.18 String.prototype.padEnd.
fn pad_end(_agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = to_string(this)?;
    let max_length = args.first().cloned().unwrap_or(Value::Undefined);
    let fill = args.get(1).cloned().unwrap_or(Value::Undefined);
    let (max, fill_string) = string_padding_impl(&s, &max_length, &fill)?;
    Ok(Value::String(Handle::new(string_pad(
        &s,
        max,
        &fill_string,
        false,
    ))))
}

/// spec 22.1.3.20 String.prototype.padStart.
fn pad_start(_agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = to_string(this)?;
    let max_length = args.first().cloned().unwrap_or(Value::Undefined);
    let fill = args.get(1).cloned().unwrap_or(Value::Undefined);
    let (max, fill_string) = string_padding_impl(&s, &max_length, &fill)?;
    Ok(Value::String(Handle::new(string_pad(
        &s,
        max,
        &fill_string,
        true,
    ))))
}

/// StringPaddingBuiltinsImpl (spec 22.1.3.20.1): ToLength the maxLength and
/// default the fill string to SPACE.
fn string_padding_impl(
    s: &JsString,
    max_length: &Value,
    fill: &Value,
) -> Result<(u64, JsString), JsError> {
    let int_max = to_length(to_number(max_length)?);
    if int_max <= s.len() as u64 {
        return Ok((int_max, JsString::from_utf8("")));
    }
    let fill_string = if matches!(fill.kind(), ValueKind::Undefined) {
        JsString::from_utf8(" ")
    } else {
        to_string(fill)?
    };
    Ok((int_max, fill_string))
}

/// StringPad (spec 22.1.3.20.2): repeat-and-truncate the filler.
fn string_pad(s: &JsString, max_length: u64, fill: &JsString, start: bool) -> JsString {
    let string_length = s.len() as u64;
    if max_length <= string_length || fill.is_empty() {
        return s.clone();
    }
    let fill_length = (max_length - string_length) as usize;
    let fill_units = fill.as_slice();
    let mut filler = Vec::with_capacity(fill_length);
    let mut i = 0;
    while filler.len() < fill_length {
        filler.push(fill_units[i % fill_units.len()]);
        i += 1;
    }
    let mut units = Vec::with_capacity(fill_length + s.len());
    if start {
        units.extend_from_slice(&filler);
        units.extend_from_slice(s.as_slice());
    } else {
        units.extend_from_slice(s.as_slice());
        units.extend_from_slice(&filler);
    }
    JsString::from_utf16(&units)
}

/// spec 22.1.3.21 String.prototype.repeat.
fn repeat(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let count = args.first().cloned().unwrap_or(Value::Undefined);
    let n = to_integer_or_infinity(crate::context::to_number(agent, &count)?);
    if n < 0.0 || n == f64::INFINITY {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Invalid count value".into(),
        ));
    }
    if n == 0.0 || s.is_empty() {
        return Ok(Value::String(Handle::new(JsString::from_utf8(""))));
    }
    let count = n as usize;
    let Some(total) = s.len().checked_mul(count) else {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "String too long".into(),
        ));
    };
    let mut units = Vec::with_capacity(total);
    for _ in 0..count {
        units.extend_from_slice(s.as_slice());
    }
    Ok(Value::String(Handle::new(JsString::from_utf16(&units))))
}

/// spec 22.1.3.22 String.prototype.replace: `@@replace` delegation, then the
/// string search with GetSubstitution.
fn replace(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let search_value = args.first().cloned().unwrap_or(Value::Undefined);
    let replace_value = args.get(1).cloned().unwrap_or(Value::Undefined);
    if matches!(
        search_value.kind(),
        ValueKind::Object(_) | ValueKind::Function(_)
    ) && let Some(replacer) = crate::expr::get_method(agent, &search_value, "@@replace")?
    {
        return crate::function::call(
            agent,
            &replacer,
            search_value,
            &[this.clone(), replace_value],
        );
    }
    let string = crate::context::to_string(agent, this)?;
    let search_string = crate::context::to_string(agent, &search_value)?;
    let functional = is_callable(&replace_value);
    let replace_text = if functional {
        None
    } else {
        Some(crate::context::to_string(agent, &replace_value)?)
    };
    let search_len = search_string.len();
    let Some(position) = string_index_of(&string, &search_string, 0) else {
        return Ok(Value::String(Handle::new(string)));
    };
    let preceding = substring(&string, 0, position);
    let following = substring(&string, position + search_len, string.len());
    let replacement = if functional {
        let called = crate::function::call(
            agent,
            &replace_value,
            Value::Undefined,
            &[
                Value::String(Handle::new(search_string.clone())),
                Value::Number(position as f64),
                Value::String(Handle::new(string)),
            ],
        )?;
        crate::context::to_string(agent, &called)?
    } else {
        get_substitution(
            agent,
            &search_string,
            &string,
            position,
            &[],
            None,
            replace_text.as_ref().unwrap_or(&JsString::from_utf8("")),
        )?
    };
    let mut units = preceding.as_slice().to_vec();
    units.extend_from_slice(replacement.as_slice());
    units.extend_from_slice(following.as_slice());
    Ok(Value::String(Handle::new(JsString::from_utf16(&units))))
}

/// spec 22.1.3.23 String.prototype.replaceAll: the non-overlapping string
/// search with `advanceBy = max(1, searchLength)`.
fn replace_all(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let search_value = args.first().cloned().unwrap_or(Value::Undefined);
    let replace_value = args.get(1).cloned().unwrap_or(Value::Undefined);
    if matches!(
        search_value.kind(),
        ValueKind::Object(_) | ValueKind::Function(_)
    ) {
        if is_regexp(agent, &search_value)? {
            let flags = get_property(
                agent,
                &search_value,
                &JsString::from_utf8("flags"),
                search_value.clone(),
            )?;
            require_object_coercible(&flags)?;
            let flag_text = crate::context::to_string(agent, &flags)?;
            if !flag_text.as_slice().contains(&(b'g' as u16)) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "String.prototype.replaceAll called with a non-global RegExp".into(),
                ));
            }
        }
        if let Some(replacer) = crate::expr::get_method(agent, &search_value, "@@replace")? {
            return crate::function::call(
                agent,
                &replacer,
                search_value,
                &[this.clone(), replace_value],
            );
        }
    }
    let string = crate::context::to_string(agent, this)?;
    let search_string = crate::context::to_string(agent, &search_value)?;
    let functional = is_callable(&replace_value);
    let replace_text = if functional {
        None
    } else {
        Some(crate::context::to_string(agent, &replace_value)?)
    };
    let search_len = search_string.len();
    let advance_by = search_len.max(1);
    let mut match_positions = Vec::new();
    let mut position = string_index_of(&string, &search_string, 0);
    while let Some(p) = position {
        match_positions.push(p);
        position = string_index_of(&string, &search_string, p + advance_by);
    }
    let mut result: Vec<u16> = Vec::new();
    let mut end_of_last_match = 0usize;
    for match_position in match_positions {
        result.extend_from_slice(substring(&string, end_of_last_match, match_position).as_slice());
        let replacement = if functional {
            let called = crate::function::call(
                agent,
                &replace_value,
                Value::Undefined,
                &[
                    Value::String(Handle::new(search_string.clone())),
                    Value::Number(match_position as f64),
                    Value::String(Handle::new(string.clone())),
                ],
            )?;
            crate::context::to_string(agent, &called)?
        } else {
            get_substitution(
                agent,
                &search_string,
                &string,
                match_position,
                &[],
                None,
                replace_text.as_ref().unwrap_or(&JsString::from_utf8("")),
            )?
        };
        result.extend_from_slice(replacement.as_slice());
        end_of_last_match = match_position + search_len;
    }
    if end_of_last_match < string.len() {
        result.extend_from_slice(substring(&string, end_of_last_match, string.len()).as_slice());
    }
    Ok(Value::String(Handle::new(JsString::from_utf16(&result))))
}

/// spec 22.1.3.24 String.prototype.search: `@@search` delegation, then
/// RegExpCreate (Phase 11).
fn search(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let regexp = args.first().cloned().unwrap_or(Value::Undefined);
    // spec 22.1.3.24 step 2: a non-Object searchValue never consults its
    // @@search property; it is coerced by RegExpCreate instead.
    if matches!(regexp.kind(), ValueKind::Object(_) | ValueKind::Function(_))
        && let Some(searcher) = crate::expr::get_method(agent, &regexp, "@@search")?
    {
        return crate::function::call(agent, &searcher, regexp, std::slice::from_ref(this));
    }
    let s = crate::context::to_string(agent, this)?;
    let rx = regexp_create(agent, &regexp, None)?;
    let searcher = crate::expr::get_method(agent, &rx, "@@search")?
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "RegExp has no @@search".into()))?;
    crate::function::call(agent, &searcher, rx, &[Value::String(Handle::new(s))])
}

/// spec 22.1.3.25 String.prototype.slice.
fn slice(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let len = s.len();
    let from = to_clamped_index(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
        len,
    )?;
    let to = match args.get(1).cloned().unwrap_or(Value::Undefined) {
        v if v.is_undefined() => len,
        other => to_clamped_index(agent, &other, len)?,
    };
    if from >= to {
        return Ok(Value::String(Handle::new(JsString::from_utf8(""))));
    }
    Ok(Value::String(Handle::new(substring(&s, from, to))))
}

/// spec 22.1.3.26 String.prototype.split: `@@split` delegation, then the pure
/// StringSplit algorithm.
fn split(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let separator = args.first().cloned().unwrap_or(Value::Undefined);
    let limit = args.get(1).cloned().unwrap_or(Value::Undefined);
    if matches!(
        separator.kind(),
        ValueKind::Object(_) | ValueKind::Function(_)
    ) && let Some(splitter) = crate::expr::get_method(agent, &separator, "@@split")?
    {
        return crate::function::call(agent, &splitter, separator, &[this.clone(), limit]);
    }
    let string = crate::context::to_string(agent, this)?;
    let lim = if matches!(limit.kind(), ValueKind::Undefined) {
        u32::MAX
    } else {
        to_uint32(crate::context::to_number(agent, &limit)?)
    };
    let separator_string = crate::context::to_string(agent, &separator)?;
    if lim == 0 {
        return array_from_list(agent, &[]);
    }
    if matches!(separator.kind(), ValueKind::Undefined) {
        return array_from_list(agent, &[string]);
    }
    let separator_len = separator_string.len();
    if separator_len == 0 {
        let string_len = string.len();
        let out_length = (lim as usize).min(string_len);
        let head = substring(&string, 0, out_length);
        let code_units: Vec<Value> = head
            .as_slice()
            .iter()
            .map(|&u| Value::String(Handle::new(JsString::from_utf16(&[u]))))
            .collect();
        return crate::builtins::array::array_from_values(agent, &code_units);
    }
    if string.is_empty() {
        return array_from_list(agent, &[string]);
    }
    let mut substrings: Vec<JsString> = Vec::new();
    let mut search_start = 0usize;
    let mut match_index = string_index_of(&string, &separator_string, 0);
    while let Some(match_position) = match_index {
        substrings.push(substring(&string, search_start, match_position));
        if substrings.len() == lim as usize {
            return array_from_list(agent, &substrings);
        }
        search_start = match_position + separator_len;
        match_index = string_index_of(&string, &separator_string, search_start);
    }
    substrings.push(substring(&string, search_start, string.len()));
    array_from_list(agent, &substrings)
}

/// spec 22.1.3.27 String.prototype.startsWith.
fn starts_with(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let search_value = args.first().cloned().unwrap_or(Value::Undefined);
    if is_regexp(agent, &search_value)? {
        return Err(regexp_argument_error("startsWith"));
    }
    let search = crate::context::to_string(agent, &search_value)?;
    let len = s.len();
    let position = args.get(1).cloned().unwrap_or(Value::Undefined);
    let start = to_integer_or_infinity(crate::context::to_number(agent, &position)?)
        .clamp(0.0, len as f64) as usize;
    let search_len = search.len();
    if search_len == 0 {
        return Ok(Value::Boolean(true));
    }
    if start + search_len > len {
        return Ok(Value::Boolean(false));
    }
    Ok(Value::Boolean(
        substring(&s, start, start + search_len) == search,
    ))
}

/// Annex B.2.3.1 String.prototype.substr.
fn substr(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let size = s.len();
    let int_start = to_clamped_index(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
        size,
    )?;
    let int_length = match args.get(1).cloned().unwrap_or(Value::Undefined) {
        v if v.is_undefined() => size,
        other => to_integer_or_infinity(crate::context::to_number(agent, &other)?)
            .clamp(0.0, size as f64) as usize,
    };
    let int_end = (int_start + int_length).min(size);
    Ok(Value::String(Handle::new(substring(
        &s, int_start, int_end,
    ))))
}

/// spec 22.1.3.28 String.prototype.substring.
fn substring_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let len = s.len();
    let final_start = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?)
    .clamp(0.0, len as f64) as usize;
    let final_end = match args.get(1).cloned().unwrap_or(Value::Undefined) {
        v if v.is_undefined() => len,
        other => to_integer_or_infinity(crate::context::to_number(agent, &other)?)
            .clamp(0.0, len as f64) as usize,
    };
    let from = final_start.min(final_end);
    let to = final_start.max(final_end);
    Ok(Value::String(Handle::new(substring(&s, from, to))))
}

/// spec 22.1.3.30 String.prototype.toLocaleLowerCase: identical to
/// `toLowerCase` under the default locale.
fn to_locale_lower_case(
    agent: &mut Agent,
    this: &Value,
    _args: &[Value],
) -> Result<Value, JsError> {
    to_lower_case(agent, this, &[])
}

/// spec 22.1.3.31 String.prototype.toLocaleUpperCase.
fn to_locale_upper_case(
    agent: &mut Agent,
    this: &Value,
    _args: &[Value],
) -> Result<Value, JsError> {
    to_upper_case(agent, this, &[])
}

/// spec 22.1.3.32 String.prototype.toLowerCase: Unicode Default Case
/// Conversion over the code points.
/// spec 22.1.3.31 String.prototype.toLowerCase: Unicode default case
/// conversion, including the Final_Sigma conditional mapping: U+03A3 maps to
/// U+03C2 when preceded by a cased character and not followed by a cased
/// character (combining marks are skipped in the lookahead).
fn to_lower_case(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let code_points: Vec<u32> = s.code_points().collect();
    let mut lower: Vec<u32> = Vec::new();
    for (index, cp) in code_points.iter().enumerate() {
        if *cp == 0x03A3 && is_final_sigma(&code_points, index) {
            lower.push(0x03C2);
        } else {
            lower.extend(unicode::to_lowercase(*cp));
        }
    }
    let text = crux::string::code_points_to_string(&lower)?;
    Ok(Value::String(Handle::new(text)))
}

/// The Final_Sigma condition: the sigma is preceded by a cased character
/// (skipping Case_Ignorable characters) and is not followed by a cased
/// character.
fn is_final_sigma(code_points: &[u32], index: usize) -> bool {
    let preceded = code_points[..index]
        .iter()
        .rev()
        .find(|cp| !is_case_ignorable(**cp))
        .is_some_and(|cp| is_cased(*cp));
    if !preceded {
        return false;
    }
    let followed_cased = code_points[index + 1..]
        .iter()
        .find(|cp| !is_case_ignorable(**cp))
        .is_some_and(|cp| is_cased(*cp));
    !followed_cased
}

/// Whether the code point has the Unicode "cased" property (general
/// category Lu, Ll, or Lt). The case mappings are not a reliable proxy:
/// Rust's tables omit the mathematical alphanumeric block, so `𝒢`
/// (U+1D4A2, Lu) must be cased by category.
fn is_cased(cp: u32) -> bool {
    matches!(unicode::general_category(cp), "Lu" | "Ll" | "Lt")
}

/// Whether the code point is Case_Ignorable (spec 3.13): the Mn/Me/Cf/Lm/Sk
/// categories plus the isolated hangul fillers, and the Final_Sigma special
/// case where FULL STOP and MIDDLE DOT are also skipped.
fn is_case_ignorable(cp: u32) -> bool {
    matches!(
        unicode::general_category(cp),
        "Mn" | "Me" | "Cf" | "Lm" | "Sk"
    ) || matches!(cp, 0x002E | 0x00B7 | 0x115F | 0x1160 | 0x3164 | 0xFFA0)
}

/// spec 22.1.3.33 String.prototype.toUpperCase.
fn to_upper_case(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let mut code_points: Vec<u32> = Vec::new();
    for cp in s.code_points() {
        code_points.extend(unicode::to_uppercase(cp));
    }
    let text = crux::string::code_points_to_string(&code_points)?;
    Ok(Value::String(Handle::new(text)))
}

/// spec 22.1.3.34 String.prototype.toString.
fn to_string_method(_agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let s = this_string_value(this)?;
    Ok(Value::String(Handle::new(s)))
}

/// spec 22.1.3.35 String.prototype.toWellFormed.
fn to_well_formed(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    let units = s.as_slice();
    let mut result: Vec<u16> = Vec::new();
    let mut k = 0;
    while k < units.len() {
        match s.code_point_at(k) {
            Some((cp, is_unpaired, count)) => {
                if is_unpaired {
                    result.push(0xFFFD);
                } else if cp <= 0xFFFF {
                    result.push(cp as u16);
                } else {
                    let x = cp - 0x10000;
                    result.push(0xD800 + (x >> 10) as u16);
                    result.push(0xDC00 + (x & 0x3FF) as u16);
                }
                k += count;
            }
            None => break,
        }
    }
    Ok(Value::String(Handle::new(JsString::from_utf16(&result))))
}

/// spec 22.1.3.36 String.prototype.trim.
fn trim(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    Ok(Value::String(Handle::new(trim_string(&s, true, true))))
}

/// spec 22.1.3.37 String.prototype.trimEnd.
fn trim_end(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    Ok(Value::String(Handle::new(trim_string(&s, false, true))))
}

/// spec 22.1.3.38 String.prototype.trimStart.
fn trim_start(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = crate::context::to_string(agent, this)?;
    Ok(Value::String(Handle::new(trim_string(&s, true, false))))
}

/// TrimString (spec 22.1.3.36.1): WhiteSpace ∪ LineTerminator from the
/// requested ends. All trim characters are BMP code units, so per-unit checks
/// are exact.
fn trim_string(s: &JsString, from_start: bool, from_end: bool) -> JsString {
    let units = s.as_slice();
    let is_trim =
        |u: &u16| unicode::is_white_space(*u as u32) || unicode::is_line_terminator(*u as u32);
    let mut from = 0;
    let mut to = units.len();
    if from_start {
        while from < to && is_trim(&units[from]) {
            from += 1;
        }
    }
    if from_end {
        while to > from && is_trim(&units[to - 1]) {
            to -= 1;
        }
    }
    JsString::from_utf16(&units[from..to])
}

/// spec 22.1.3.39 String.prototype.valueOf.
fn value_of(_agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let s = this_string_value(this)?;
    Ok(Value::String(Handle::new(s)))
}

/// spec 22.1.3.40 String.prototype[@@iterator]: a fresh String iterator.
fn string_iterator(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let s = to_string(this)?;
    let realm = agent.current_realm()?;
    let proto = realm
        .intrinsics
        .get(STRING_ITERATOR)
        .and_then(|value| as_object(&value));
    let object = JsObject::ordinary_object_create(proto);
    agent.string_iter_data.insert(object.id(), (Some(s), 0));
    Ok(Value::Object(object))
}

/// spec 22.1.5.2.1 %StringIteratorPrototype%.next.
fn string_iterator_next(
    agent: &mut Agent,
    this: &Value,
    _args: &[Value],
) -> Result<Value, JsError> {
    let ValueKind::Object(obj) = this.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "%StringIteratorPrototype%.next called on an incompatible receiver".into(),
        ));
    };
    let Some((string, next_index)) = agent.string_iter_data.get_mut(&obj.id()) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "%StringIteratorPrototype%.next called on an incompatible receiver".into(),
        ));
    };
    let Some(s) = string else {
        return iter_result(Value::Undefined, true);
    };
    let len = s.len();
    if *next_index >= len as u64 {
        *string = None;
        return iter_result(Value::Undefined, true);
    }
    let position = *next_index as usize;
    let (_, _, count) = s.code_point_at(position).unwrap_or((0, false, 1));
    let result_string = substring(s, position, position + count);
    *next_index = (position + count) as u64;
    iter_result(Value::String(Handle::new(result_string)), false)
}

/// CreateIterResultObject (spec 7.4.9).
fn iter_result(value: Value, done: bool) -> Result<Value, JsError> {
    let object = JsObject::ordinary_object_create(None);
    object.create_data_property(&JsString::from_utf8("value"), value)?;
    object.create_data_property(&JsString::from_utf8("done"), Value::Boolean(done))?;
    Ok(Value::Object(object))
}

/// CreateArrayFromList of strings.
fn array_from_list(agent: &mut Agent, items: &[JsString]) -> Result<Value, JsError> {
    let values: Vec<Value> = items
        .iter()
        .map(|s| Value::String(Handle::new(s.clone())))
        .collect();
    crate::builtins::array::array_from_values(agent, &values)
}

/// spec 22.1.3.17.1 GetSubstitution: the `$`-directive replacement template
/// expansion. `captures` is empty for string patterns; named-group captures
/// join with RegExp in Phase 11.
#[allow(clippy::too_many_arguments)]
fn get_substitution(
    agent: &mut Agent,
    matched: &JsString,
    string: &JsString,
    position: usize,
    captures: &[Option<JsString>],
    named_captures: Option<&Handle<JsObject>>,
    template: &JsString,
) -> Result<JsString, JsError> {
    let units = template.as_slice();
    let mut result: Vec<u16> = Vec::new();
    let mut q = 0usize;
    while q < units.len() {
        if units[q] != b'$' as u16 {
            result.push(units[q]);
            q += 1;
            continue;
        }
        match units.get(q + 1).copied() {
            None => {
                result.push(b'$' as u16);
                q += 1;
            }
            Some(u) if u == b'$' as u16 => {
                result.push(b'$' as u16);
                q += 2;
            }
            Some(u) if u == b'&' as u16 => {
                result.extend_from_slice(matched.as_slice());
                q += 2;
            }
            Some(u) if u == b'`' as u16 => {
                result.extend_from_slice(&string.as_slice()[..position.min(string.len())]);
                q += 2;
            }
            Some(u) if u == b'\'' as u16 => {
                let tail = (position + matched.len()).min(string.len());
                result.extend_from_slice(&string.as_slice()[tail..]);
                q += 2;
            }
            Some(u) if (0x30..=0x39).contains(&u) => {
                let mut digit_count = 1;
                if let Some(&second) = units.get(q + 2)
                    && (0x30..=0x39).contains(&second)
                {
                    digit_count = 2;
                }
                let mut index = (u - 0x30) as usize;
                if digit_count == 2 {
                    index = index * 10 + (units[q + 2] - 0x30) as usize;
                }
                let capture_length = captures.len();
                if index > capture_length && digit_count == 2 {
                    digit_count = 1;
                    index = (u - 0x30) as usize;
                }
                if (1..=capture_length).contains(&index) {
                    if let Some(capture) = &captures[index - 1] {
                        result.extend_from_slice(capture.as_slice());
                    }
                    q += 1 + digit_count;
                } else {
                    result.push(b'$' as u16);
                    q += 1;
                }
            }
            Some(u) if u == b'<' as u16 => {
                let gt = (q + 2..units.len())
                    .position(|i| units[i] == b'>' as u16)
                    .map(|offset| q + 2 + offset);
                match gt {
                    None => {
                        result.push(b'$' as u16);
                        result.push(b'<' as u16);
                        q += 2;
                    }
                    Some(gt_position) => {
                        let Some(named) = named_captures else {
                            result.push(b'$' as u16);
                            result.push(b'<' as u16);
                            q += 2;
                            continue;
                        };
                        let name = JsString::from_utf16(&units[q + 2..gt_position]);
                        let capture = get_property(
                            agent,
                            &Value::Object(named.clone()),
                            &name,
                            Value::Object(named.clone()),
                        )?;
                        if !matches!(capture.kind(), ValueKind::Undefined) {
                            result.extend_from_slice(to_string(&capture)?.as_slice());
                        }
                        q = gt_position + 1;
                    }
                }
            }
            Some(_) => {
                result.push(b'$' as u16);
                q += 1;
            }
        }
    }
    Ok(JsString::from_utf16(&result))
}

/// IsRegExp (spec 7.2.9): an object with a [[RegExpMatcher]] slot or a
/// truthy `@@match` property.
pub(crate) fn is_regexp(agent: &mut Agent, value: &Value) -> Result<bool, JsError> {
    if !matches!(value.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Ok(false);
    }
    // spec IsRegExp: the @@match property takes precedence over the
    // [[RegExpMatcher]] slot (an explicit false makes the object a non-Regexp).
    let key = PropertyKey::Symbol(crux::symbol::well_known("match").as_ref().clone());
    let matcher = get_property_key(agent, value, &key, value.clone())?;
    if matches!(matcher.kind(), ValueKind::Undefined) {
        if let ValueKind::Object(obj) = value.kind()
            && agent.regexp_data.contains_key(&obj.id())
        {
            return Ok(true);
        }
        return Ok(false);
    }
    Ok(crux::convert::to_boolean(&matcher))
}

/// The `GetSubstitution` variant used by `RegExp.prototype[@@replace]`: the
/// captures are language values (converted here) and the named captures are
/// an object or `undefined`.
pub(crate) fn get_substitution_public(
    agent: &mut Agent,
    matched: &JsString,
    string: &JsString,
    position: usize,
    captures: &[Value],
    named_captures: Option<Value>,
    template: &JsString,
) -> Result<JsString, JsError> {
    let capture_strings: Vec<Option<JsString>> = captures
        .iter()
        .map(|c| match c.kind() {
            ValueKind::Undefined => Ok(None),
            _ => Ok(Some(crate::context::to_string(agent, c)?)),
        })
        .collect::<Result<Vec<_>, JsError>>()?;
    let named = named_captures.and_then(|value| value.as_object());
    get_substitution(
        agent,
        matched,
        string,
        position,
        &capture_strings,
        named.as_ref(),
        template,
    )
}

/// RegExpCreate (spec 22.2.4.6): construct via `%RegExp%` with the given
/// pattern and flags.
fn regexp_create(
    agent: &mut Agent,
    pattern: &Value,
    flags: Option<&str>,
) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let ctor = realm
        .intrinsics
        .get("%RegExp%")
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%RegExp% is not defined".into()))?;
    let args: Vec<Value> = match flags {
        Some(flags) => vec![
            pattern.clone(),
            Value::String(Handle::new(JsString::from_utf8(flags))),
        ],
        None => vec![pattern.clone()],
    };
    crate::function::construct(agent, &ctor, &args, &ctor)
}

fn regexp_argument_error(method: &str) -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        format!("First argument to String.prototype.{method} must not be a regular expression"),
    )
}

fn eq_ascii(s: &JsString, text: &str) -> bool {
    s.as_slice() == text.encode_utf16().collect::<Vec<u16>>().as_slice()
}

/// Install the String intrinsics and the global `String` binding (spec 22.1)
/// during SetDefaultGlobalBindings. `%Object.prototype%` must exist first
/// (`%String.prototype%` is a String exotic wrapping the empty string).
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let string_proto = JsObject::string_create(JsString::from_utf8(""), object_proto.clone())?;
    let string_proto_value = Value::Object(string_proto.clone());

    let string_ctor = Function::create_builtin(
        Some(JsString::from_utf8("String")),
        1,
        placeholder("String"),
        Some(Box::new(placeholder("String"))),
        None,
    )?;
    let string_ctor_value = Value::Function(string_ctor.clone());

    realm.intrinsics.define(STRING, string_ctor_value.clone());
    realm
        .intrinsics
        .define(STRING_PROTO, string_proto_value.clone());

    string_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(string_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    string_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(string_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // spec 22.1.2: the pure statics run as native closures. They are not
    // registered as intrinsics, so the realm post-pass cannot link their
    // [[Prototype]]; set %Function.prototype% here (CreateBuiltinFunction,
    // spec 10.2.3 step 1) so `.call`/`.apply`/`.bind` resolve.
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    let statics: [(&str, u64, StringFn); 2] = [
        ("fromCharCode", 1, from_char_code),
        ("fromCodePoint", 1, from_code_point),
    ];
    for (name, length, body) in statics {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(body),
            None,
            function_proto.clone(),
        )?;
        string_ctor.define_property(
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
    // String.raw needs the agent (array-like property reads).
    let raw_func = Function::create_builtin(
        Some(JsString::from_utf8("raw")),
        1,
        placeholder("raw"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(RAW, Value::Function(raw_func.clone()));
    string_ctor.define_property(
        &JsString::from_utf8("raw"),
        &PropertyDescriptor {
            value: Some(Value::Function(raw_func)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // spec 22.1.3: the prototype methods, all agent-dispatched (ToString on
    // object receivers and the RegExp delegations need the agent).
    let methods: [(&str, &str, u64); 40] = [
        ("at", AT, 1),
        ("charAt", CHAR_AT, 1),
        ("charCodeAt", CHAR_CODE_AT, 1),
        ("codePointAt", CODE_POINT_AT, 1),
        ("concat", CONCAT, 1),
        ("endsWith", ENDS_WITH, 1),
        ("includes", INCLUDES, 1),
        ("indexOf", INDEX_OF, 1),
        ("isWellFormed", IS_WELL_FORMED, 0),
        ("lastIndexOf", LAST_INDEX_OF, 1),
        ("localeCompare", LOCALE_COMPARE, 1),
        ("match", MATCH, 1),
        ("matchAll", MATCH_ALL, 1),
        ("normalize", NORMALIZE, 0),
        ("padEnd", PAD_END, 1),
        ("padStart", PAD_START, 1),
        ("repeat", REPEAT, 1),
        ("replace", REPLACE, 2),
        ("replaceAll", REPLACE_ALL, 2),
        ("search", SEARCH, 1),
        ("slice", SLICE, 2),
        ("split", SPLIT, 2),
        ("startsWith", STARTS_WITH, 1),
        ("substr", SUBSTR, 2),
        ("substring", SUBSTRING, 2),
        ("toLocaleLowerCase", TO_LOCALE_LOWER_CASE, 0),
        ("toLocaleUpperCase", TO_LOCALE_UPPER_CASE, 0),
        ("toLowerCase", TO_LOWER_CASE, 0),
        ("toUpperCase", TO_UPPER_CASE, 0),
        ("toString", TO_STRING, 0),
        ("toWellFormed", TO_WELL_FORMED, 0),
        ("trim", TRIM, 0),
        ("trimEnd", TRIM_END, 0),
        ("trimStart", TRIM_START, 0),
        ("valueOf", VALUE_OF, 0),
        ("anchor", "%String.prototype.anchor%", 1),
        ("big", "%String.prototype.big%", 0),
        ("blink", "%String.prototype.blink%", 0),
        ("bold", "%String.prototype.bold%", 0),
        ("fixed", "%String.prototype.fixed%", 0),
    ];
    for (name, key, length) in methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(func.clone()));
        string_proto.define_property(
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
    // Annex B.3.1: trimLeft/trimRight are the *same function objects* as
    // trimStart/trimEnd (reference-trimStart.js checks the identities).
    for (alias, target) in [("trimLeft", TRIM_START), ("trimRight", TRIM_END)] {
        if let Some(shared) = realm.intrinsics.get(target) {
            string_proto.define_property(
                &JsString::from_utf8(alias),
                &PropertyDescriptor {
                    value: Some(shared),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(false),
                    configurable: Some(true),
                },
            )?;
        }
    }
    let html_wrappers: [(&str, &str, u64); 8] = [
        ("fontcolor", "%String.prototype.fontcolor%", 1),
        ("fontsize", "%String.prototype.fontsize%", 1),
        ("italics", "%String.prototype.italics%", 0),
        ("link", "%String.prototype.link%", 1),
        ("small", "%String.prototype.small%", 0),
        ("strike", "%String.prototype.strike%", 0),
        ("sub", "%String.prototype.sub%", 0),
        ("sup", "%String.prototype.sup%", 0),
    ];
    for (name, key, length) in html_wrappers {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(func.clone()));
        string_proto.define_property(
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

    // spec 22.1.3.40: String.prototype[@@iterator].
    let iterator_func = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.iterator]")),
        0,
        placeholder("[Symbol.iterator]"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(ITERATOR, Value::Function(iterator_func.clone()));
    string_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(iterator_func)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // spec 22.1.5: %StringIteratorPrototype%.
    let iterator_proto = JsObject::ordinary_object_create(object_proto.clone());
    let iterator_proto_value = Value::Object(iterator_proto.clone());
    realm
        .intrinsics
        .define(STRING_ITERATOR, iterator_proto_value.clone());
    let next_func = Function::create_builtin(
        Some(JsString::from_utf8("next")),
        0,
        placeholder("next"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(STRING_ITERATOR_NEXT, Value::Function(next_func.clone()));
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
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "String Iterator",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("String"),
        &PropertyDescriptor {
            value: Some(string_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The String members that need the agent, dispatched by intrinsic identity
/// from `runtime::function::call`/`construct`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(STRING).as_ref() == Some(callee) {
        return Some(string_call(agent, args));
    }
    if intrinsics.get(RAW).as_ref() == Some(callee) {
        return Some(raw(agent, this, args));
    }
    if intrinsics.get(AT).as_ref() == Some(callee) {
        return Some(at(agent, this, args));
    }
    if intrinsics.get(CHAR_AT).as_ref() == Some(callee) {
        return Some(char_at(agent, this, args));
    }
    if intrinsics.get(CHAR_CODE_AT).as_ref() == Some(callee) {
        return Some(char_code_at(agent, this, args));
    }
    if intrinsics.get(CODE_POINT_AT).as_ref() == Some(callee) {
        return Some(code_point_at(agent, this, args));
    }
    if intrinsics.get(CONCAT).as_ref() == Some(callee) {
        return Some(concat(agent, this, args));
    }
    if intrinsics.get(ENDS_WITH).as_ref() == Some(callee) {
        return Some(ends_with(agent, this, args));
    }
    if intrinsics.get(INCLUDES).as_ref() == Some(callee) {
        return Some(includes(agent, this, args));
    }
    if intrinsics.get(INDEX_OF).as_ref() == Some(callee) {
        return Some(index_of(agent, this, args));
    }
    if intrinsics.get(IS_WELL_FORMED).as_ref() == Some(callee) {
        return Some(is_well_formed(agent, this, args));
    }
    if intrinsics.get(LAST_INDEX_OF).as_ref() == Some(callee) {
        return Some(last_index_of(agent, this, args));
    }
    if intrinsics.get(LOCALE_COMPARE).as_ref() == Some(callee) {
        return Some(locale_compare(agent, this, args));
    }
    if intrinsics.get(MATCH).as_ref() == Some(callee) {
        return Some(match_method(agent, this, args));
    }
    if intrinsics.get(MATCH_ALL).as_ref() == Some(callee) {
        return Some(match_all(agent, this, args));
    }
    if intrinsics.get(NORMALIZE).as_ref() == Some(callee) {
        return Some(normalize(agent, this, args));
    }
    if intrinsics.get(PAD_END).as_ref() == Some(callee) {
        return Some(pad_end(agent, this, args));
    }
    if intrinsics.get(PAD_START).as_ref() == Some(callee) {
        return Some(pad_start(agent, this, args));
    }
    if intrinsics.get(REPEAT).as_ref() == Some(callee) {
        return Some(repeat(agent, this, args));
    }
    if intrinsics.get(REPLACE).as_ref() == Some(callee) {
        return Some(replace(agent, this, args));
    }
    if intrinsics.get(REPLACE_ALL).as_ref() == Some(callee) {
        return Some(replace_all(agent, this, args));
    }
    if intrinsics.get(SEARCH).as_ref() == Some(callee) {
        return Some(search(agent, this, args));
    }
    if intrinsics.get(SLICE).as_ref() == Some(callee) {
        return Some(slice(agent, this, args));
    }
    if intrinsics.get(SPLIT).as_ref() == Some(callee) {
        return Some(split(agent, this, args));
    }
    if intrinsics.get(STARTS_WITH).as_ref() == Some(callee) {
        return Some(starts_with(agent, this, args));
    }
    if intrinsics.get(SUBSTR).as_ref() == Some(callee) {
        return Some(substr(agent, this, args));
    }
    if intrinsics.get(SUBSTRING).as_ref() == Some(callee) {
        return Some(substring_method(agent, this, args));
    }
    if intrinsics.get(TO_LOCALE_LOWER_CASE).as_ref() == Some(callee) {
        return Some(to_locale_lower_case(agent, this, args));
    }
    if intrinsics.get(TO_LOCALE_UPPER_CASE).as_ref() == Some(callee) {
        return Some(to_locale_upper_case(agent, this, args));
    }
    if intrinsics.get(TO_LOWER_CASE).as_ref() == Some(callee) {
        return Some(to_lower_case(agent, this, args));
    }
    if intrinsics.get(TO_UPPER_CASE).as_ref() == Some(callee) {
        return Some(to_upper_case(agent, this, args));
    }
    if intrinsics.get(TO_STRING).as_ref() == Some(callee) {
        return Some(to_string_method(agent, this, args));
    }
    if intrinsics.get(TO_WELL_FORMED).as_ref() == Some(callee) {
        return Some(to_well_formed(agent, this, args));
    }
    if intrinsics.get(TRIM).as_ref() == Some(callee) {
        return Some(trim(agent, this, args));
    }
    if intrinsics.get(TRIM_END).as_ref() == Some(callee) {
        return Some(trim_end(agent, this, args));
    }
    if intrinsics.get(TRIM_START).as_ref() == Some(callee) {
        return Some(trim_start(agent, this, args));
    }
    if intrinsics.get(VALUE_OF).as_ref() == Some(callee) {
        return Some(value_of(agent, this, args));
    }
    if intrinsics.get(ITERATOR).as_ref() == Some(callee) {
        return Some(string_iterator(agent, this, args));
    }
    if intrinsics.get(STRING_ITERATOR_NEXT).as_ref() == Some(callee) {
        return Some(string_iterator_next(agent, this, args));
    }
    // The Annex B HTML wrappers.
    for (key, tag, attr, value_index) in [
        ("%String.prototype.anchor%", "a", Some("name"), 0),
        ("%String.prototype.big%", "big", None, 0),
        ("%String.prototype.blink%", "blink", None, 0),
        ("%String.prototype.bold%", "b", None, 0),
        ("%String.prototype.fixed%", "tt", None, 0),
        ("%String.prototype.fontcolor%", "font", Some("color"), 0),
        ("%String.prototype.fontsize%", "font", Some("size"), 0),
        ("%String.prototype.italics%", "i", None, 0),
        ("%String.prototype.link%", "a", Some("href"), 0),
        ("%String.prototype.small%", "small", None, 0),
        ("%String.prototype.strike%", "strike", None, 0),
        ("%String.prototype.sub%", "sub", None, 0),
        ("%String.prototype.sup%", "sup", None, 0),
    ] {
        if intrinsics.get(key).as_ref() == Some(callee) {
            return Some(create_html(agent, this, tag, attr, value_index, args));
        }
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
    if realm.intrinsics.get(STRING).as_ref() == Some(callee) {
        return Some(string_construct(agent, args, new_target));
    }
    None
}

/// CreateHTML (spec B.2.3.2.1): the Annex B wrapper with `"` escaping in the
/// attribute value.
fn create_html(
    _agent: &mut Agent,
    this: &Value,
    tag: &str,
    attr: Option<&str>,
    value_index: usize,
    args: &[Value],
) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let contents = to_string(this)?;
    let mut out = format!("<{tag}");
    if let Some(attr) = attr {
        let attr_value = to_string(&args.get(value_index).cloned().unwrap_or(Value::Undefined))?;
        let escaped: Vec<u16> = attr_value
            .as_slice()
            .iter()
            .flat_map(|&u| {
                if u == b'"' as u16 {
                    vec![
                        b'&' as u16,
                        b'q' as u16,
                        b'u' as u16,
                        b'o' as u16,
                        b't' as u16,
                        b';' as u16,
                    ]
                } else {
                    vec![u]
                }
            })
            .collect();
        out.push_str(&format!(
            " {attr}=\"{}\"",
            JsString::from_utf16(&escaped).to_string_lossy()
        ));
    }
    out.push('>');
    out.push_str(&contents.to_string_lossy());
    out.push_str(&format!("</{tag}>"));
    Ok(Value::String(Handle::new(JsString::from_utf8(&out))))
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
    fn constructor_forms() {
        assert_eq!(text("String()"), "");
        assert_eq!(text("String(42)"), "42");
        assert_eq!(text("String(null)"), "null");
        assert_eq!(text("String(undefined)"), "undefined");
        assert_eq!(text("String(true)"), "true");
        assert_eq!(run("typeof String").unwrap().to_string(), "function");
        // new String boxes with a length and indexed access.
        assert_eq!(number("new String('ab').length"), 2.0);
        assert_eq!(text("new String('ab')[1]"), "b");
        assert_eq!(text("new String('ab').toString()"), "ab");
        assert_eq!(text("new String('ab').valueOf()"), "ab");
        assert_eq!(text("String.prototype.toString()"), "");
        assert_eq!(text("String.prototype.valueOf()"), "");
        assert_eq!(number("String.prototype.length"), 0.0);
    }

    #[test]
    fn statics() {
        assert_eq!(text("String.fromCharCode()"), "");
        assert_eq!(text("String.fromCharCode(72, 105)"), "Hi");
        assert_eq!(text("String.fromCharCode(0x1F600)"), "\u{f600}");
        assert_eq!(text("String.fromCharCode(65537)"), "\u{1}");
        assert_eq!(text("String.fromCodePoint()"), "");
        assert_eq!(text("String.fromCodePoint(72)"), "H");
        assert_eq!(text("String.fromCodePoint(0x1F600)"), "\u{1F600}");
        assert_eq!(text("String.fromCodePoint(65, 66)"), "AB");
        assert!(matches!(
            run("String.fromCodePoint(-1)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("String.fromCodePoint(0x110000)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("String.fromCodePoint(1.5)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert_eq!(text("String.raw`a${1}b${2}c`"), "a1b2c");
        assert_eq!(text("String.raw`plain`"), "plain");
        assert_eq!(text("String.raw`\\n`"), "\\n");
        assert_eq!(text("String.raw({ raw: ['a', 'b'] }, 1)"), "a1b");
        assert_eq!(text("String.raw({ raw: [] })"), "");
        assert_eq!(text("String.raw`${1}`"), "1");
    }

    #[test]
    fn search_methods() {
        assert_eq!(number("('hello').indexOf('l')"), 2.0);
        assert_eq!(number("('hello').indexOf('l', 3)"), 3.0);
        assert_eq!(number("('hello').indexOf('x')"), -1.0);
        assert_eq!(number("('hello').indexOf('')"), 0.0);
        assert_eq!(number("('abc').lastIndexOf('b')"), 1.0);
        assert_eq!(number("('abcabc').lastIndexOf('bc')"), 4.0);
        assert_eq!(number("('abc').lastIndexOf('b', 0)"), -1.0);
        assert_eq!(number("('abc').lastIndexOf('')"), 3.0);
        assert!(bool("('hello').includes('ell')"));
        assert!(!bool("('hello').includes('ell', 2)"));
        assert!(bool("('hello').includes('')"));
        assert!(bool("('hello').startsWith('he')"));
        assert!(!bool("('hello').startsWith('he', 2)"));
        assert!(bool("('hello').startsWith('', 5)"));
        assert!(bool("('hello').endsWith('lo')"));
        assert!(!bool("('hello').endsWith('lo', 4)"));
        assert!(bool("('hello').endsWith('', 0)"));
        assert!(matches!(
            run("String.prototype.includes.call(null, 'x')"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    fn extraction_methods() {
        assert_eq!(text("('hello').charAt(1)"), "e");
        assert_eq!(text("('hello').charAt(99)"), "");
        assert_eq!(text("('hello').charAt(-1)"), "");
        assert_eq!(number("('hello').charCodeAt(1)"), 101.0);
        assert!(number("('hello').charCodeAt(99)").is_nan());
        assert_eq!(number("('a\u{1F600}b').codePointAt(1)"), 128512.0);
        assert_eq!(number("('a\u{1F600}b').codePointAt(2)"), 0xDE00 as f64);
        assert!(matches!(
            run("('abc').codePointAt(9)"),
            Ok(v) if v.is_undefined()
        ));
        assert_eq!(text("('hello').at(1)"), "e");
        assert_eq!(text("('hello').at(-1)"), "o");
        assert!(matches!(
            run("('hello').at(99)"),
            Ok(v) if v.is_undefined()
        ));
        assert_eq!(text("('hello').concat(' ', 'world')"), "hello world");
        assert_eq!(text("('abc').slice(1)"), "bc");
        assert_eq!(text("('abc').slice(1, 2)"), "b");
        assert_eq!(text("('abc').slice(-2)"), "bc");
        assert_eq!(text("('abc').slice(-3, -1)"), "ab");
        assert_eq!(text("('abc').slice(2, 1)"), "");
        assert_eq!(text("('abc').substring(1, 3)"), "bc");
        assert_eq!(text("('abc').substring(3, 1)"), "bc");
        assert_eq!(text("('abc').substring(-5, 2)"), "ab");
        assert_eq!(text("('abc').substring(9)"), "");
        assert_eq!(text("('abc').substring(-9)"), "abc");
        assert_eq!(text("('abc').substr(1)"), "bc");
        assert_eq!(text("('abc').substr(1, 1)"), "b");
        assert_eq!(text("('abc').substr(-2)"), "bc");
        assert_eq!(text("('abc').substr(1, 0)"), "");
        assert_eq!(text("String.prototype.slice.call(5, 1)"), "");
    }

    #[test]
    fn repeat_and_padding() {
        assert_eq!(text("('ab').repeat(3)"), "ababab");
        assert_eq!(text("('ab').repeat(0)"), "");
        assert_eq!(text("('').repeat(5)"), "");
        assert!(matches!(
            run("('ab').repeat(-1)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("('ab').repeat(Infinity)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert_eq!(text("('7').padStart(3)"), "  7");
        assert_eq!(text("('7').padStart(3, '0')"), "007");
        assert_eq!(text("('7').padStart(5, 'ab')"), "abab7");
        assert_eq!(text("('abc').padStart(2)"), "abc");
        assert_eq!(text("('7').padEnd(3, '0')"), "700");
        assert_eq!(text("('7').padEnd(5, 'xy')"), "7xyxy");
        assert_eq!(text("('7').padEnd(3, '')"), "7");
        // Padding counts code units, so an astral fill truncates mid-pair.
        assert_eq!(text("('x').padStart(4, '\\u{1F600}')"), "😀\u{FFFD}x");
    }

    #[test]
    fn case_conversion() {
        assert_eq!(text("('Hello World').toLowerCase()"), "hello world");
        assert_eq!(text("('Hello World').toUpperCase()"), "HELLO WORLD");
        assert_eq!(text("('ß').toUpperCase()"), "SS");
        assert_eq!(text("('İ').toLowerCase()"), "i̇");
        assert_eq!(text("('abc').toLocaleLowerCase()"), "abc");
        assert_eq!(text("('abc').toLocaleUpperCase()"), "ABC");
        assert_eq!(text("String.prototype.toLowerCase.call(123)"), "123");
        // Lone surrogates pass through unchanged (rendered lossy below).
        assert_eq!(text("('\\uD800A').toLowerCase()"), "\u{FFFD}a");
    }

    #[test]
    fn normalization() {
        assert_eq!(text("('é').normalize()"), "é");
        assert_eq!(text("('e\u{301}').normalize('NFC')"), "é");
        assert_eq!(text("('é').normalize('NFD')"), "e\u{301}");
        assert_eq!(text("('Ａ').normalize('NFKC')"), "A");
        assert_eq!(text("('Ａ').normalize('NFKD')"), "A");
        assert_eq!(text("('\u{FB01}').normalize('NFKC')"), "fi");
        assert_eq!(text("('\u{FB01}').normalize('NFC')"), "\u{FB01}");
        assert!(matches!(
            run("('abc').normalize('NOPE')"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn substitution() {
        assert_eq!(text("('abc').replace('b', 'X')"), "aXc");
        assert_eq!(text("('abc').replace('x', 'X')"), "abc");
        assert_eq!(text("('abc').replace('', 'X')"), "Xabc");
        assert_eq!(text("('aXaXa').replaceAll('X', 'b')"), "ababa");
        assert_eq!(text("('aaa').replaceAll('', 'X')"), "XaXaXaX");
        assert_eq!(text("('abc').replace('b', '$$')"), "a$c");
        assert_eq!(text("('abc').replace('b', '$&$&')"), "abbc");
        assert_eq!(text("('abc').replace('b', \"$`\")"), "aac");
        assert_eq!(text("('abc').replace('b', \"$'\")"), "acc");
        assert_eq!(text("('abc').replace('b', '$1')"), "a$1c");
        assert_eq!(text("('abc').replace('b', '$10')"), "a$10c");
        assert_eq!(text("('abc').replace('b', '$<name>')"), "a$<name>c");
        assert_eq!(
            text("('a1b2c').replaceAll('1', 'X').replaceAll('2', 'Y')"),
            "aXbYc"
        );
    }

    #[test]
    fn splitting() {
        assert_eq!(number("('a,b,c').split(',').length"), 3.0);
        assert_eq!(text("('a,b,c').split(',')[1]"), "b");
        assert_eq!(number("('a,b,c').split(',', 2).length"), 2.0);
        assert_eq!(number("('abc').split('').length"), 3.0);
        assert_eq!(text("('abc').split('')[1]"), "b");
        assert_eq!(number("('').split('').length"), 0.0);
        assert_eq!(number("('').split(',').length"), 1.0);
        assert_eq!(number("('abc').split(undefined).length"), 1.0);
        assert_eq!(text("('abc').split(undefined)[0]"), "abc");
        assert_eq!(number("('abc').split('x').length"), 1.0);
        assert_eq!(number("('abab').split('ab').length"), 3.0);
        assert_eq!(number("('a,b').split(',', 0).length"), 0.0);
        assert_eq!(number("('a𝌆b').split('').length"), 4.0);
    }

    #[test]
    fn trimming() {
        assert_eq!(text("('  x  ').trim()"), "x");
        assert_eq!(text("('\\t\\n x\\r\\u{2028} ').trim()"), "x");
        assert_eq!(text("('  x  ').trimStart()"), "x  ");
        assert_eq!(text("('  x  ').trimEnd()"), "  x");
        assert_eq!(text("('x').trim()"), "x");
        assert_eq!(text("('').trim()"), "");
        assert_eq!(text("('\u{FEFF}x\u{FEFF}').trim()"), "x");
        assert_eq!(text("('\u{00A0}x\u{00A0}').trim()"), "x");
    }

    #[test]
    fn well_formedness() {
        assert!(bool("('abc').isWellFormed()"));
        assert!(bool("('\\u{1F600}').isWellFormed()"));
        assert!(!bool("('\\uD800').isWellFormed()"));
        assert!(!bool("('\\uDFFF').isWellFormed()"));
        assert_eq!(
            text("('\\uD800a\\uDFFFb').toWellFormed()"),
            "\u{FFFD}a\u{FFFD}b"
        );
        assert_eq!(text("('\\u{1F600}').toWellFormed()"), "\u{1F600}");
    }

    #[test]
    fn iterator_protocol() {
        assert_eq!(
            number("var s = 'ab'; s[Symbol.iterator] === undefined ? 0 : 1"),
            1.0
        );
        assert_eq!(
            text("var it = 'ab'[Symbol.iterator](); it.next().value"),
            "a"
        );
        assert_eq!(
            text(
                "var it = 'a\u{1F600}b'; var i = it[Symbol.iterator](); i.next().value + i.next().value + i.next().value"
            ),
            "a\u{1F600}b"
        );
        assert_eq!(
            text(
                "var it = 'x'[Symbol.iterator](); it.next(); var d = it.next(); String(d.done) + String(d.value)"
            ),
            "trueundefined"
        );
        assert_eq!(
            text("Object.prototype.toString.call('ab'[Symbol.iterator]())"),
            "[object String Iterator]"
        );
    }

    #[test]
    fn html_wrappers() {
        assert_eq!(text("('x').anchor('a\"b')"), "<a name=\"a&quot;b\">x</a>");
        assert_eq!(text("('x').big()"), "<big>x</big>");
        assert_eq!(text("('x').bold()"), "<b>x</b>");
        assert_eq!(text("('x').fixed()"), "<tt>x</tt>");
        assert_eq!(
            text("('x').fontcolor('red')"),
            "<font color=\"red\">x</font>"
        );
        assert_eq!(text("('x').fontsize('3')"), "<font size=\"3\">x</font>");
        assert_eq!(text("('x').italics()"), "<i>x</i>");
        assert_eq!(text("('x').link('http://a')"), "<a href=\"http://a\">x</a>");
        assert_eq!(text("('x').small()"), "<small>x</small>");
        assert_eq!(text("('x').strike()"), "<strike>x</strike>");
        assert_eq!(text("('x').sub()"), "<sub>x</sub>");
        assert_eq!(text("('x').sup()"), "<sup>x</sup>");
        assert_eq!(text("('x').blink()"), "<blink>x</blink>");
    }

    #[test]
    fn locale_compare_and_generic() {
        assert_eq!(number("('a').localeCompare('a')"), 0.0);
        assert_eq!(number("('a').localeCompare('b')"), -1.0);
        assert_eq!(number("('b').localeCompare('a')"), 1.0);
        assert_eq!(text("String.prototype.concat.call(5, 6)"), "56");
        assert_eq!(number("String.prototype.indexOf.call('abc', 'b')"), 1.0);
        assert!(matches!(
            run("String.prototype.trim.call(null)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert!(matches!(
            run("String.prototype.charAt.call(undefined)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert!(matches!(
            run("String.prototype.toString.call(5)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert_eq!(
            text("Object.prototype.toString.call(new String('x'))"),
            "[object String]"
        );
        assert_eq!(
            text("Object.prototype.toString.call('x')"),
            "[object String]"
        );
    }

    #[test]
    fn case_mapping_expansion() {
        // ß expands to two code points in the full uppercase mapping.
        assert_eq!(text("('stra\u{00DF}e').toUpperCase()"), "STRASSE");
        // U+03A3 alone lowercases to the plain sigma (no final-sigma context).
        assert_eq!(text("('\u{03A3}').toLowerCase()"), "σ");
        // The LATIN SMALL LIGATURE FI uppercases to the two letters FI.
        assert_eq!(text("('\u{FB01}').toUpperCase()"), "FI");
        assert_eq!(text("('abc').toUpperCase()"), "ABC");
        // Round trip through both conversions.
        assert_eq!(
            text("('Hello World').toLowerCase().toUpperCase()"),
            "HELLO WORLD"
        );
    }

    #[test]
    fn final_sigma_contextual_mapping() {
        // U+03A3 maps to the final sigma (U+03C2) only when preceded by a
        // cased character and not followed by one (spec 22.1.3.31).
        assert_eq!(
            text("('\u{039F}\u{03A3}').toLowerCase()"),
            "\u{03BF}\u{03C2}"
        );
        // A medial sigma stays the plain sigma.
        assert_eq!(
            text("('\u{039F}\u{03A3}\u{0394}').toLowerCase()"),
            "\u{03BF}\u{03C3}\u{03B4}"
        );
        // A word-initial sigma is not final.
        assert_eq!(
            text("('\u{03A3}\u{03BF}').toLowerCase()"),
            "\u{03C3}\u{03BF}"
        );
    }

    #[test]
    fn string_of_symbol_uses_descriptive_string() {
        // spec 22.1.1.1 step 2: String(Symbol) is SymbolDescriptiveString.
        assert_eq!(text("String(Symbol('x'))"), "Symbol(x)");
        assert_eq!(text("String(Symbol())"), "Symbol()");
        assert_eq!(text("String(Symbol.iterator)"), "Symbol(Symbol.iterator)");
        // Concatenation still throws for symbols.
        assert!(matches!(
            run("'' + Symbol('x')"),
            Err(error) if error.kind == crux::ErrorKind::TypeError
        ));
    }

    #[test]
    fn split_edge_cases() {
        assert_eq!(text("('abc').split('').join('|')"), "a|b|c");
        assert_eq!(text("('abc').split('', 2).join('|')"), "a|b");
        assert_eq!(text("('a,b,c').split(',', 2).join('|')"), "a|b");
        assert_eq!(text("('abc').split(',').join('|')"), "abc");
        assert_eq!(text("('a,b').split(undefined).join('|')"), "a,b");
        assert_eq!(text("('').split(',').join('|')"), "");
        // A regexp separator ends with a match, so the trailing empty string
        // is part of the result (spec 22.2.7.5 step 17).
        assert_eq!(number("('a1b2c3').split(/\\d/).length"), 4.0);
        assert_eq!(text("('a1b2c3').split(/\\d/)[0]"), "a");
        assert_eq!(text("('a1b2c3').split(/\\d/)[1]"), "b");
        assert_eq!(text("('a1b2c3').split(/\\d/)[2]"), "c");
        assert_eq!(text("('a1b2c3').split(/\\d/)[3]"), "");
        // Capturing groups in a regexp separator are spliced into the result.
        assert_eq!(number("('ab').split(/(.)/).length"), 5.0);
        assert_eq!(text("('ab').split(/(.)/)[0]"), "");
        assert_eq!(text("('ab').split(/(.)/)[1]"), "a");
        assert_eq!(text("('ab').split(/(.)/)[2]"), "");
        assert_eq!(text("('ab').split(/(.)/)[3]"), "b");
        assert_eq!(text("('ab').split(/(.)/)[4]"), "");
    }

    #[test]
    fn replace_substitution_patterns() {
        assert_eq!(text("('abc').replace('b', 'X$&Y')"), "aXbYc");
        // $$ is a literal dollar in the replacement template.
        assert_eq!(text("('a1b').replace(/\\d/, '$$')"), "a$b");
        // Numbered captures can be reordered in the template.
        assert_eq!(text("('ab').replace(/(a)(b)/, '$2$1')"), "ba");
        assert_eq!(text("('aaa').replaceAll('a', 'x')"), "xxx");
        // replaceAll requires a global regexp.
        assert!(matches!(
            run("('aaa').replaceAll(/a/, 'x')"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    fn padding_edge_cases() {
        assert_eq!(text("('5').padStart(3, '0')"), "005");
        assert_eq!(text("('5').padStart(1, '0')"), "5");
        assert_eq!(text("('ab').padEnd(4, 'xy')"), "abxy");
        // The filler is truncated when it does not divide evenly.
        assert_eq!(text("('ab').padStart(4, 'xyz')"), "xyab");
        assert_eq!(text("('').padStart(3)"), "   ");
    }

    #[test]
    fn slice_substring_edge_cases() {
        assert_eq!(text("('hello').slice(-3)"), "llo");
        assert_eq!(text("('hello').slice(1, -1)"), "ell");
        // substring swaps a reversed argument pair.
        assert_eq!(text("('hello').substring(3, 1)"), "el");
        // Negative positions clamp to zero.
        assert_eq!(text("('hello').substring(-5, 2)"), "he");
    }

    #[test]
    fn surrogate_code_point_edge_cases() {
        assert_eq!(number("('\\u{1F600}').charCodeAt(0)"), 0xD83D as f64);
        assert_eq!(number("('\\u{1F600}').codePointAt(0)"), 128512.0);
        assert_eq!(text("String.fromCodePoint(128512)"), "\u{1F600}");
        assert_eq!(text("String.fromCharCode(0xD83D, 0xDE00)"), "\u{1F600}");
        assert_eq!(text("('abc').charAt(5)"), "");
        assert!(number("('abc').charCodeAt(5)").is_nan());
    }

    #[test]
    fn search_trim_edge_cases() {
        assert_eq!(number("('abcabc').indexOf('abc', 1)"), 3.0);
        assert!(bool("('abc').includes('')"));
        assert!(!bool("('abc').includes('x', 10)"));
        assert!(bool("('abc').startsWith('')"));
        assert!(bool("('abc').endsWith('')"));
        // NBSP and BOM are both whitespace for trimming.
        assert_eq!(text("('\\u{00A0}\\u{FEFF} x \\u{00A0}').trim()"), "x");
    }

    #[test]
    fn length_and_stringification() {
        // Length counts UTF-16 code units, so an astral char is two units.
        assert_eq!(number("('\\u{1F600}').length"), 2.0);
        assert_eq!(number("('').length"), 0.0);
        assert_eq!(text("String(1e21)"), "1e+21");
        assert_eq!(text("String(-0)"), "0");
        // An array stringifies via its join.
        assert_eq!(text("String([1, 2])"), "1,2");
        // A Symbol has no string conversion (throws even in concatenation).
        assert!(matches!(
            run("'' + Symbol('x')"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }
}
