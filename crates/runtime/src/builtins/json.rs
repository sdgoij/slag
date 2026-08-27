//! The JSON built-in (spec 26.6): a JSON grammar parser producing ECMAScript
//! values with the ES2026 reviver context (`context.source` for unmodified
//! primitives), the full `stringify` pipeline (toJSON, replacer, space,
//! quoting, cycle detection), and the ES2026 `rawJSON`/`isRawJSON` pair.

use crux::convert::{to_length, to_number, to_string as value_to_string};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::ops::same_value;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_callable};

use crate::agent::Agent;
use crate::context::{as_object, get_property};
use crate::realm::Realm;

const JSON_NS: &str = "%JSON%";
const JSON_PARSE: &str = "%JSON.parse%";
const JSON_STRINGIFY: &str = "%JSON.stringify%";
const JSON_RAW_JSON: &str = "%JSON.rawJSON%";
const JSON_IS_RAW_JSON: &str = "%JSON.isRawJSON%";

/// One parsed JSON value: the produced language value plus the parse-record
/// children used by the ES2026 reviver context.
struct ParseRecord {
    value: Value,
    /// The raw source text; only primitives carry one (the reviver context's
    /// `source` is set only for unmodified non-object values).
    source: Option<JsString>,
    /// Element records, in order, for arrays.
    elements: Vec<ParseRecord>,
    /// Entry records, keyed by property name, for objects.
    entries: Vec<(JsString, ParseRecord)>,
}

/// The recursive-descent JSON grammar (ECMA-404): JSONValue over UTF-8 bytes.
struct JsonParser<'a> {
    agent: &'a mut Agent,
    text: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn syntax_error(&self) -> JsError {
        JsError::new(
            ErrorKind::SyntaxError,
            format!("Unexpected token at position {}", self.pos),
        )
    }

    fn skip_ws(&mut self) {
        while let Some(&byte) = self.text.get(self.pos) {
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.text.get(self.pos) == Some(&byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &[u8]) -> bool {
        if self.text.get(self.pos..self.pos + word.len()) == Some(word) {
            self.pos += word.len();
            true
        } else {
            false
        }
    }

    fn object_proto(&self) -> Option<Handle<JsObject>> {
        self.agent
            .current_realm()
            .ok()
            .and_then(|realm| realm.intrinsics.get("%Object.prototype%"))
            .and_then(|value| as_object(&value))
    }

    /// Parse a JSONValue; primitives record the raw source range so the
    /// reviver context can reproduce it.
    fn parse_value(&mut self) -> Result<ParseRecord, JsError> {
        self.skip_ws();
        let start = self.pos;
        let value = match self.text.get(self.pos).copied() {
            Some(b'"') => Value::String(Handle::new(self.parse_string()?)),
            Some(b'{') => return self.parse_object_record(),
            Some(b'[') => return self.parse_array_record(),
            Some(b't') if self.eat_word(b"true") => Value::Boolean(true),
            Some(b'f') if self.eat_word(b"false") => Value::Boolean(false),
            Some(b'n') if self.eat_word(b"null") => Value::Null,
            Some(b'-') | Some(b'0'..=b'9') => Value::Number(self.parse_number()?),
            _ => return Err(self.syntax_error()),
        };
        let end = self.pos;
        let source = match value.kind() {
            ValueKind::Object(_) | ValueKind::Function(_) => None,
            _ => Some(JsString::from_utf8(
                std::str::from_utf8(&self.text[start..end]).unwrap_or(""),
            )),
        };
        Ok(ParseRecord {
            value,
            source,
            elements: Vec::new(),
            entries: Vec::new(),
        })
    }

    /// `{ members }`: JSONObject with the member records kept for the reviver.
    fn parse_object_record(&mut self) -> Result<ParseRecord, JsError> {
        let start = self.pos;
        self.eat(b'{');
        let object = JsObject::ordinary_object_create(self.object_proto());
        let mut entries = Vec::new();
        self.skip_ws();
        if self.eat(b'}') {
            let _ = start;
            return Ok(ParseRecord {
                value: Value::Object(object),
                source: None,
                elements: Vec::new(),
                entries,
            });
        }
        loop {
            self.skip_ws();
            if !self.eat(b'"') {
                return Err(self.syntax_error());
            }
            self.pos -= 1;
            let key = self.parse_string()?;
            self.skip_ws();
            if !self.eat(b':') {
                return Err(self.syntax_error());
            }
            let value = self.parse_value()?;
            object.create_data_property_or_throw(&key, value.value)?;
            entries.push((key, value));
            self.skip_ws();
            if self.eat(b'}') {
                break;
            }
            if !self.eat(b',') {
                return Err(self.syntax_error());
            }
        }
        Ok(ParseRecord {
            value: Value::Object(object),
            source: None,
            elements: Vec::new(),
            entries,
        })
    }

    /// `[ elements ]`: JSONArray with the element records kept.
    fn parse_array_record(&mut self) -> Result<ParseRecord, JsError> {
        self.eat(b'[');
        let mut elements = Vec::new();
        self.skip_ws();
        if self.eat(b']') {
            let array = crate::builtins::array::array_create(self.agent, 0.0)?;
            return Ok(ParseRecord {
                value: Value::Object(array),
                source: None,
                elements,
                entries: Vec::new(),
            });
        }
        loop {
            let value = self.parse_value()?;
            elements.push(value);
            self.skip_ws();
            if self.eat(b']') {
                break;
            }
            if !self.eat(b',') {
                return Err(self.syntax_error());
            }
        }
        let array = crate::builtins::array::array_create(self.agent, elements.len() as f64)?;
        for (index, element) in elements.iter().enumerate() {
            array.create_data_property_or_throw(
                &JsString::from_utf8(&index.to_string()),
                element.value,
            )?;
        }
        Ok(ParseRecord {
            value: Value::Object(array),
            source: None,
            elements,
            entries: Vec::new(),
        })
    }

    fn parse_string(&mut self) -> Result<JsString, JsError> {
        self.skip_ws();
        if !self.eat(b'"') {
            return Err(self.syntax_error());
        }
        let mut units: Vec<u16> = Vec::new();
        loop {
            let Some(&byte) = self.text.get(self.pos) else {
                return Err(self.syntax_error());
            };
            self.pos += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(&escape) = self.text.get(self.pos) else {
                        return Err(self.syntax_error());
                    };
                    self.pos += 1;
                    match escape {
                        b'"' => units.push(b'"' as u16),
                        b'\\' => units.push(b'\\' as u16),
                        b'/' => units.push(b'/' as u16),
                        b'b' => units.push(0x08),
                        b'f' => units.push(0x0C),
                        b'n' => units.push(b'\n' as u16),
                        b'r' => units.push(b'\r' as u16),
                        b't' => units.push(b'\t' as u16),
                        b'u' => {
                            let hi = self.parse_hex4()?;
                            units.push(hi);
                            if (0xD800..=0xDBFF).contains(&hi)
                                && self.text.get(self.pos..self.pos + 2) == Some(b"\\u")
                            {
                                let save = self.pos + 2;
                                self.pos = save;
                                let lo = self.parse_hex4()?;
                                if (0xDC00..=0xDFFF).contains(&lo) {
                                    units.push(lo);
                                } else {
                                    self.pos = save - 2;
                                }
                            }
                        }
                        _ => return Err(self.syntax_error()),
                    }
                }
                0x00..=0x1F => return Err(self.syntax_error()),
                _ => {
                    // Multi-byte UTF-8: decode the code point and push its
                    // UTF-16 encoding (a surrogate pair for astral code
                    // points).
                    let len = utf8_len(byte);
                    let end = (self.pos - 1 + len).min(self.text.len());
                    let chunk = &self.text[self.pos - 1..end];
                    if let Ok(text) = std::str::from_utf8(chunk) {
                        self.pos = end;
                        for unit in text.encode_utf16() {
                            units.push(unit);
                        }
                    } else {
                        return Err(self.syntax_error());
                    }
                }
            }
        }
        Ok(JsString::from_utf16(&units))
    }

    fn parse_hex4(&mut self) -> Result<u16, JsError> {
        if self.pos + 4 > self.text.len() {
            return Err(self.syntax_error());
        }
        let mut value = 0u16;
        for _ in 0..4 {
            let Some(&byte) = self.text.get(self.pos) else {
                return Err(self.syntax_error());
            };
            self.pos += 1;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.syntax_error()),
            };
            value = value * 16 + digit as u16;
        }
        Ok(value)
    }

    /// JSONNumber: `-? int frac? exp?` with no leading zeros and no `+`.
    fn parse_number(&mut self) -> Result<f64, JsError> {
        let start = self.pos;
        self.eat(b'-');
        match self.text.get(self.pos).copied() {
            Some(b'0') => {
                self.pos += 1;
            }
            Some(b'1'..=b'9') => {
                while matches!(self.text.get(self.pos), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.syntax_error()),
        }
        if self.eat(b'.') {
            let frac_start = self.pos;
            while matches!(self.text.get(self.pos), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == frac_start {
                return Err(self.syntax_error());
            }
        }
        if matches!(self.text.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.text.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let exp_start = self.pos;
            while matches!(self.text.get(self.pos), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == exp_start {
                return Err(self.syntax_error());
            }
        }
        let text = std::str::from_utf8(&self.text[start..self.pos]).unwrap_or("");
        // The JSON number grammar is a subset of the JS numeric-literal
        // grammar, so the shared string→number conversion applies unchanged.
        to_number(&Value::String(Handle::new(JsString::from_utf8(text))))
            .map_err(|_| self.syntax_error())
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
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

fn str(text: &str) -> Value {
    Value::String(Handle::new(JsString::from_utf8(text)))
}

/// IsRawJSON (spec 26.6.4): an object registered in the raw-JSON table.
fn is_raw_json(agent: &Agent, value: &Value) -> bool {
    match value.kind() {
        ValueKind::Object(obj) => agent.raw_json_data.contains_key(&obj.id()),
        _ => false,
    }
}

/// The [[RawJSON]] text of a raw-JSON object.
fn raw_json_source(agent: &Agent, value: &Value) -> Option<JsString> {
    match value.kind() {
        ValueKind::Object(obj) => agent.raw_json_data.get(&obj.id()).cloned(),
        _ => None,
    }
}

/// StringToJSONPrimitive (spec 26.6.3.1): the text is exactly one JSON
/// primitive (string, number, boolean, or null); `None` otherwise.
fn json_primitive_value(agent: &mut Agent, text: &JsString) -> Result<Option<Value>, JsError> {
    let bytes = text.to_string_lossy().into_bytes();
    let mut parser = JsonParser {
        agent,
        text: &bytes,
        pos: 0,
    };
    let record = match parser.parse_value() {
        Ok(record) => record,
        Err(_) => return Ok(None),
    };
    if !matches!(
        record.value.kind(),
        ValueKind::Object(_) | ValueKind::Function(_)
    ) {
        parser.skip_ws();
        if parser.pos == parser.text.len() {
            return Ok(Some(record.value));
        }
    }
    Ok(None)
}

/// ToString of the first argument (spec 7.1.17); Symbols throw.
fn to_string_arg(agent: &mut Agent, value: &Value) -> Result<JsString, JsError> {
    value_to_string(&crate::context::to_primitive(
        agent,
        value,
        crux::convert::ToPrimitiveHint::String,
    )?)
}

/// Validate that `text` is well-formed JSON (used by JSON modules, which
/// must reject invalid sources at resolution time with a SyntaxError, spec
/// 16.2.1.7.1 ParseModule for JSON modules). The parsed value is discarded.
pub(crate) fn validate_json(agent: &mut Agent, text: &str) -> Result<(), JsError> {
    let bytes = text.as_bytes();
    let mut parser = JsonParser {
        agent,
        text: bytes,
        pos: 0,
    };
    parser.parse_value()?;
    parser.skip_ws();
    if parser.pos != parser.text.len() {
        return Err(parser.syntax_error());
    }
    Ok(())
}

/// JSON.parse (spec 26.6.2): parse the text, then internalize through the
/// reviver with the ES2026 parse-record context.
fn json_parse(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    // GC-2: the ParseRecord tree (and its per-element/per-entry record Vecs)
    // sits in native heap buffers the stack scan cannot see while parsing
    // and the reviver (user code) allocate — suppress `--gc-stress` for the
    // whole operation so the records cannot be swept out from under the
    // internalize recursion.
    let _stress = crate::ir::StressSuppress::new();
    let _ = this;
    let text_value = args.first().cloned().unwrap_or(Value::Undefined);
    let text = to_string_arg(agent, &text_value)?;
    let bytes = text.to_string_lossy().into_bytes();
    let mut parser = JsonParser {
        agent,
        text: &bytes,
        pos: 0,
    };
    let record = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos != parser.text.len() {
        return Err(parser.syntax_error());
    }
    let reviver = args.get(1).cloned().unwrap_or(Value::Undefined);
    if !is_callable(&reviver) {
        return Ok(record.value);
    }
    let root = JsObject::ordinary_object_create(
        agent
            .current_realm()?
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| as_object(&value)),
    );
    root.create_data_property_or_throw(&JsString::from_utf8(""), record.value)?;
    let result = internalize_json_property(agent, &root, "", &reviver, Some(&record))?;
    Ok(result)
}

/// InternalizeJSONProperty (spec 26.6.2.3): recurse into the value, then call
/// the reviver with the parse-record context object.
fn internalize_json_property(
    agent: &mut Agent,
    holder: &Handle<JsObject>,
    name: &str,
    reviver: &Value,
    record: Option<&ParseRecord>,
) -> Result<Value, JsError> {
    let key = PropertyKey::from_utf8(name);
    let holder_value = Value::Object(*holder);
    let value = crate::context::get_property_key(agent, &holder_value, &key, holder_value)?;
    let context = JsObject::ordinary_object_create(
        agent
            .current_realm()?
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| as_object(&value)),
    );
    let (elements, entries) = match record {
        Some(record) if same_value(&record.value, &value) => {
            if !matches!(value.kind(), ValueKind::Object(_) | ValueKind::Function(_))
                && let Some(source) = &record.source
            {
                context.create_data_property_or_throw(
                    &JsString::from_utf8("source"),
                    str(&source.to_string_lossy()),
                )?;
            }
            (&record.elements[..], &record.entries[..])
        }
        _ => (&[][..], &[][..]),
    };
    if let ValueKind::Object(obj) = value.kind() {
        if crate::builtins::array::is_array(&value) {
            let length = length_of_array_like(agent, &value)?;
            for index in 0..length {
                let index_key = JsString::from_utf8(&index.to_string());
                let element_record = elements.get(index as usize);
                let new_element = internalize_json_property(
                    agent,
                    &obj,
                    &index_key.to_string_lossy(),
                    reviver,
                    element_record,
                )?;
                if matches!(new_element.kind(), ValueKind::Undefined) {
                    obj.delete_key(&PropertyKey::from_js_string(&index_key))?;
                } else {
                    // spec step 28.a.3: CreateDataProperty is silent — a
                    // reviver that froze/non-configurified the property keeps
                    // the old value (reviver-*-non-configurable-prop-create).
                    obj.create_data_property(&index_key, new_element)?;
                }
            }
        } else {
            let keys = enumerable_string_keys(agent, &value)?;
            for key in keys {
                let entry_record = entries
                    .iter()
                    .find(|(entry_key, _)| entry_key == &key)
                    .map(|(_, record)| record);
                let new_element = internalize_json_property(
                    agent,
                    &obj,
                    &key.to_string_lossy(),
                    reviver,
                    entry_record,
                )?;
                if matches!(new_element.kind(), ValueKind::Undefined) {
                    obj.delete_key(&PropertyKey::from_js_string(&key))?;
                } else {
                    obj.create_data_property(&key, new_element)?;
                }
            }
        }
    }
    let result = crate::function::call(
        agent,
        reviver,
        Value::Object(*holder),
        &[str(name), value, Value::Object(context)],
    )?;
    Ok(result)
}

/// LengthOfArrayLike (spec 7.3.22).
fn length_of_array_like(agent: &mut Agent, value: &Value) -> Result<u64, JsError> {
    let length = get_property(agent, value, &JsString::from_utf8("length"), *value)?;
    Ok(to_length(to_number(&length)?))
}

/// EnumerableOwnPropertyNames (spec 7.3.23) restricted to string keys.
fn enumerable_string_keys(agent: &mut Agent, value: &Value) -> Result<Vec<JsString>, JsError> {
    let object = crate::context::to_object(agent, value)?;
    let obj = as_object(&object)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "value is not an object".into()))?;
    let mut out = Vec::new();
    for key in obj.own_property_keys()? {
        let PropertyKey::String(id) = key else {
            continue;
        };
        if let Some(prop) = obj.get_own_property_key(&PropertyKey::String(id))?
            && prop.enumerable
        {
            out.push(crux::lookup(id));
        }
    }
    Ok(out)
}

/// The stringify state (spec 26.6.3.1).
struct StringifyState {
    stack: Vec<Value>,
    replacer_function: Option<Value>,
    property_list: Option<Vec<JsString>>,
    gap: String,
    indent: String,
}

/// JSON.stringify (spec 26.6.3).
fn json_stringify(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let replacer = args.get(1).cloned().unwrap_or(Value::Undefined);
    let space = args.get(2).cloned().unwrap_or(Value::Undefined);

    let (replacer_function, property_list) = match replacer.kind() {
        ValueKind::Object(_) | ValueKind::Function(_) => {
            if is_callable(&replacer) {
                (Some(replacer), None)
            } else {
                // spec 26.6.3.1 step 4.b.i: IsArray on a revoked proxy
                // throws a TypeError.
                if let Some(obj) = replacer.as_object()
                    && let crux::object::ObjectKind::Proxy(slots) = &obj.kind
                    && slots.target.borrow().is_none()
                {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Cannot perform operation on a revoked Proxy".into(),
                    ));
                }
                if crate::builtins::array::is_array(&replacer) {
                    (None, Some(property_list_from(agent, &replacer)?))
                } else {
                    (None, None)
                }
            }
        }
        _ => (None, None),
    };

    let gap = match space.kind() {
        ValueKind::Object(obj) => {
            // spec 26.6.3.1 steps 8-10: only wrappers with [[NumberData]]
            // or [[StringData]] are converted (honoring overrides); any
            // other object is ignored.
            if agent.number_data.contains_key(&obj.id()) {
                space_string(&Value::Number(crate::context::to_number(agent, &space)?))?
            } else if matches!(obj.kind, crux::object::ObjectKind::String(_)) {
                space_string(&Value::String(Handle::new(crate::context::to_string(
                    agent, &space,
                )?)))?
            } else {
                String::new()
            }
        }
        _ => space_string(&space)?,
    };

    let mut state = StringifyState {
        stack: Vec::new(),
        replacer_function,
        property_list,
        gap,
        indent: String::new(),
    };
    let wrapper = JsObject::ordinary_object_create(
        agent
            .current_realm()?
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| as_object(&value)),
    );
    wrapper.create_data_property_or_throw(&JsString::from_utf8(""), value)?;
    let result = serialize_json_property(agent, &mut state, "", &Value::Object(wrapper))?;
    Ok(result
        .map(|text| Value::String(Handle::new(text)))
        .unwrap_or(Value::Undefined))
}

/// The `space` argument → the gap string (spec 26.6.3.1 steps 8-11).
fn space_string(space: &Value) -> Result<String, JsError> {
    match space.kind() {
        ValueKind::Number(n) => {
            let count = if n.is_finite() && n > 0.0 {
                (n.floor() as usize).min(10)
            } else {
                0
            };
            Ok(" ".repeat(count))
        }
        ValueKind::String(s) => {
            let text = s.to_string_lossy();
            Ok(text.chars().take(10).collect())
        }
        ValueKind::Object(_) => Ok(String::new()),
        _ => Ok(String::new()),
    }
}

/// The replacer array → the whitelist of unique string keys (spec
/// 26.6.3.1 steps 5-7).
fn property_list_from(agent: &mut Agent, replacer: &Value) -> Result<Vec<JsString>, JsError> {
    let length = length_of_array_like(agent, replacer)?;
    let mut list: Vec<JsString> = Vec::new();
    for index in 0..length {
        let item = get_property(
            agent,
            replacer,
            &JsString::from_utf8(&index.to_string()),
            *replacer,
        )?;
        match item.kind() {
            ValueKind::String(s) => {
                if !list.contains(&s) {
                    list.push(s.as_ref().clone());
                }
            }
            ValueKind::Number(n) => {
                let text = value_to_string(&Value::Number(n))?;
                if !list.contains(&text) {
                    list.push(text);
                }
            }
            ValueKind::Object(obj) => {
                // spec 26.6.3.1 step 5.e: an object with [[StringData]] or
                // [[NumberData]] is coerced via ToString (honoring
                // overrides); any other object is ignored.
                let has_slot = agent.number_data.contains_key(&obj.id())
                    || matches!(obj.kind, crux::object::ObjectKind::String(_));
                if has_slot {
                    let text = crate::context::to_string(agent, &item)?;
                    if !list.contains(&text) {
                        list.push(text);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(list)
}

/// SerializeJSONProperty (spec 26.6.3.2): `None` means the value is omitted.
fn serialize_json_property(
    agent: &mut Agent,
    state: &mut StringifyState,
    key: &str,
    holder: &Value,
) -> Result<Option<JsString>, JsError> {
    let key_string = JsString::from_utf8(key);
    let mut value = get_property(agent, holder, &key_string, *holder)?;
    if matches!(
        value.kind(),
        ValueKind::Object(_) | ValueKind::Function(_) | ValueKind::BigInt(_)
    ) {
        let to_json = get_property(agent, &value, &JsString::from_utf8("toJSON"), value)?;
        if is_callable(&to_json) {
            value = crate::function::call(agent, &to_json, value, &[str(key)])?;
        }
    }
    if let Some(replacer_function) = &state.replacer_function {
        value =
            crate::function::call(agent, replacer_function, *holder, &[str(key), value])?;
    }
    if let ValueKind::Object(obj) = value.kind() {
        if is_raw_json(agent, &value) {
            let source = raw_json_source(agent, &value).unwrap_or_else(|| JsString::from_utf8(""));
            return Ok(Some(source));
        }
        // Unbox the Number/String/Boolean/BigInt wrappers (spec 26.6.3.2
        // steps 4.a-d). Number and String wrappers convert through the
        // agent (ToNumber/ToString honor overridden valueOf/toString);
        // Boolean/BigInt wrappers serialize their stored primitive.
        if agent.number_data.contains_key(&obj.id()) {
            value = Value::Number(crate::context::to_number(agent, &value)?);
        } else if let Some(b) = agent.boolean_data.get(&obj.id()) {
            value = Value::Boolean(*b);
        } else if matches!(obj.kind, crux::object::ObjectKind::String(_)) {
            value = Value::String(Handle::new(crate::context::to_string(agent, &value)?));
        } else if let Some(big) = agent.bigint_data.get(&obj.id()) {
            value = Value::BigInt(Handle::new(big.clone()));
        }
    }
    match value.kind() {
        ValueKind::Null => Ok(Some(JsString::from_utf8("null"))),
        ValueKind::Boolean(true) => Ok(Some(JsString::from_utf8("true"))),
        ValueKind::Boolean(false) => Ok(Some(JsString::from_utf8("false"))),
        ValueKind::String(s) => Ok(Some(quote_json_string(&s))),
        ValueKind::Number(n) => {
            if n.is_finite() {
                Ok(Some(crux::number::to_string(n)))
            } else {
                Ok(Some(JsString::from_utf8("null")))
            }
        }
        ValueKind::BigInt(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Do not know how to serialize a BigInt".into(),
        )),
        // Only non-callable objects are serialized; callable objects,
        // undefined, and symbols are omitted (spec steps 15-16).
        ValueKind::Object(_) | ValueKind::Function(_) if !is_callable(&value) => {
            if crate::builtins::array::is_array(&value) {
                serialize_json_array(agent, state, &value)
            } else {
                serialize_json_object(agent, state, &value)
            }
        }
        _ => Ok(None),
    }
}

/// SerializeJSONObject (spec 26.6.3.4).
fn serialize_json_object(
    agent: &mut Agent,
    state: &mut StringifyState,
    value: &Value,
) -> Result<Option<JsString>, JsError> {
    if state.stack.contains(value) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Converting circular structure to JSON".into(),
        ));
    }
    state.stack.push(*value);
    let stepback = state.indent.clone();
    state.indent = format!("{}{}", state.indent, state.gap);
    let keys = match &state.property_list {
        Some(list) => list.clone(),
        None => enumerable_string_keys(agent, value)?,
    };
    let mut parts: Vec<String> = Vec::new();
    for key in &keys {
        let result = serialize_json_property(agent, state, &key.to_string_lossy(), value)?;
        if let Some(text) = result {
            let mut member = quote_json_string(key).to_string_lossy().to_string();
            member.push(':');
            if !state.gap.is_empty() {
                member.push(' ');
            }
            member.push_str(&text.to_string_lossy());
            parts.push(member);
        }
    }
    let inner_indent = state.indent.clone();
    state.indent = stepback.clone();
    state.stack.pop();
    if parts.is_empty() {
        Ok(Some(JsString::from_utf8("{}")))
    } else if state.gap.is_empty() {
        Ok(Some(JsString::from_utf8(&format!(
            "{{{}}}",
            parts.join(",")
        ))))
    } else {
        let separator = format!(",\n{}", inner_indent);
        Ok(Some(JsString::from_utf8(&format!(
            "{{\n{}{}\n{}}}",
            inner_indent,
            parts.join(&separator),
            stepback
        ))))
    }
}

/// SerializeJSONArray (spec 26.6.3.5).
fn serialize_json_array(
    agent: &mut Agent,
    state: &mut StringifyState,
    value: &Value,
) -> Result<Option<JsString>, JsError> {
    if state.stack.contains(value) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Converting circular structure to JSON".into(),
        ));
    }
    state.stack.push(*value);
    let stepback = state.indent.clone();
    state.indent = format!("{}{}", state.indent, state.gap);
    let length = length_of_array_like(agent, value)?;
    let mut parts: Vec<String> = Vec::new();
    for index in 0..length {
        let result = serialize_json_property(agent, state, &index.to_string(), value)?;
        parts.push(
            result
                .map(|text| text.to_string_lossy())
                .unwrap_or_else(|| "null".to_string()),
        );
    }
    let inner_indent = state.indent.clone();
    state.indent = stepback.clone();
    state.stack.pop();
    if state.gap.is_empty() {
        Ok(Some(JsString::from_utf8(&format!("[{}]", parts.join(",")))))
    } else {
        let separator = format!(",\n{}", inner_indent);
        Ok(Some(JsString::from_utf8(&format!(
            "[\n{}{}\n{}]",
            inner_indent,
            parts.join(&separator),
            stepback
        ))))
    }
}

/// QuoteJSONString (spec 26.6.3.6): escape quotes, backslashes, control
/// characters, and lone surrogates.
fn quote_json_string(s: &JsString) -> JsString {
    let mut out = Vec::with_capacity(s.len() + 2);
    out.push(b'"' as u16);
    let units = s.as_slice();
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        match unit {
            0x22 => {
                out.extend_from_slice(&[b'\\' as u16, b'"' as u16]);
            }
            0x5C => {
                out.extend_from_slice(&[b'\\' as u16, b'\\' as u16]);
            }
            0x08 => {
                out.extend_from_slice(&[b'\\' as u16, b'b' as u16]);
            }
            0x0C => {
                out.extend_from_slice(&[b'\\' as u16, b'f' as u16]);
            }
            0x0A => {
                out.extend_from_slice(&[b'\\' as u16, b'n' as u16]);
            }
            0x0D => {
                out.extend_from_slice(&[b'\\' as u16, b'r' as u16]);
            }
            0x09 => {
                out.extend_from_slice(&[b'\\' as u16, b't' as u16]);
            }
            0x00..=0x1F => {
                out.extend_from_slice(&[
                    b'\\' as u16,
                    b'u' as u16,
                    hex_digit(unit >> 12),
                    hex_digit((unit >> 8) & 0xF),
                    hex_digit((unit >> 4) & 0xF),
                    hex_digit(unit & 0xF),
                ]);
            }
            0xD800..=0xDBFF => {
                // A valid surrogate pair is emitted verbatim; a lone high
                // surrogate gets the \uXXXX escape.
                if let Some(&low) = units.get(index + 1)
                    && (0xDC00..=0xDFFF).contains(&low)
                {
                    out.push(unit);
                    out.push(low);
                    index += 1;
                } else {
                    out.extend_from_slice(&[
                        b'\\' as u16,
                        b'u' as u16,
                        hex_digit(unit >> 12),
                        hex_digit((unit >> 8) & 0xF),
                        hex_digit((unit >> 4) & 0xF),
                        hex_digit(unit & 0xF),
                    ]);
                }
            }
            0xDC00..=0xDFFF => {
                out.extend_from_slice(&[
                    b'\\' as u16,
                    b'u' as u16,
                    hex_digit(unit >> 12),
                    hex_digit((unit >> 8) & 0xF),
                    hex_digit((unit >> 4) & 0xF),
                    hex_digit(unit & 0xF),
                ]);
            }
            _ => out.push(unit),
        }
        index += 1;
    }
    out.push(b'"' as u16);
    JsString::from_utf16(&out)
}

fn hex_digit(value: u16) -> u16 {
    match value {
        0..=9 => b'0' as u16 + value,
        _ => b'a' as u16 + value - 10,
    }
}

/// JSON.rawJSON (spec 26.6.5): `ToString` the text, validate it is one JSON
/// primitive, and return a frozen null-prototype RawJSON object whose
/// `rawJSON` data property holds the text verbatim.
fn raw_json(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let text_value = args.first().cloned().unwrap_or(Value::Undefined);
    let text = to_string_arg(agent, &text_value)?;
    let units = text.as_slice();
    let first = units.first().copied();
    let last = units.last().copied();
    let starts_ok = first.is_some_and(|unit| {
        (0x61..=0x7A).contains(&unit)
            || (0x30..=0x39).contains(&unit)
            || unit == b'"' as u16
            || unit == b'-' as u16
    });
    let ends_ok = last.is_some_and(|unit| {
        (0x61..=0x7A).contains(&unit) || (0x30..=0x39).contains(&unit) || unit == b'"' as u16
    });
    if !starts_ok || !ends_ok {
        return Err(JsError::new(
            ErrorKind::SyntaxError,
            "Invalid JSON primitive".into(),
        ));
    }
    if json_primitive_value(agent, &text)?.is_none() {
        return Err(JsError::new(
            ErrorKind::SyntaxError,
            "Invalid JSON primitive".into(),
        ));
    }
    // spec 26.6.5: a RawJSON object has a null prototype, an [[IsRawJSON]]
    // internal slot, a `rawJSON` data property, and is frozen.
    let object = JsObject::ordinary_object_create(None);
    object.create_data_property_or_throw(
        &JsString::from_utf8("rawJSON"),
        str(&text.to_string_lossy()),
    )?;
    object.prevent_extensions()?;
    let raw_json_prop = PropertyDescriptor {
        value: Some(str(&text.to_string_lossy())),
        writable: Some(false),
        get: None,
        set: None,
        enumerable: Some(true),
        configurable: Some(false),
    };
    object.define_property(&JsString::from_utf8("rawJSON"), &raw_json_prop)?;
    agent.raw_json_data.insert(object.id(), text.clone());
    Ok(Value::Object(object))
}

/// JSON.isRawJSON (spec 26.6.4).
fn is_raw_json_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    Ok(Value::Boolean(is_raw_json(agent, &value)))
}

pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let json_object = JsObject::ordinary_object_create(
        realm
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| as_object(&value)),
    );
    realm.intrinsics.define(JSON_NS, Value::Object(json_object));

    let methods: [(&str, &str, u64); 4] = [
        ("parse", JSON_PARSE, 2),
        ("stringify", JSON_STRINGIFY, 3),
        ("rawJSON", JSON_RAW_JSON, 1),
        ("isRawJSON", JSON_IS_RAW_JSON, 1),
    ];
    for (name, intrinsic, length) in methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(intrinsic, Value::Function(func));
        json_object.define_property(
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
    json_object.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag")),
        &PropertyDescriptor {
            value: Some(str("JSON")),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("JSON"),
        &PropertyDescriptor {
            value: Some(Value::Object(json_object)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The JSON members that need the agent, dispatched by intrinsic identity
/// from `runtime::function::call`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(JSON_PARSE).as_ref() == Some(callee) {
        return Some(json_parse(agent, this, args));
    }
    if intrinsics.get(JSON_STRINGIFY).as_ref() == Some(callee) {
        return Some(json_stringify(agent, this, args));
    }
    if intrinsics.get(JSON_RAW_JSON).as_ref() == Some(callee) {
        return Some(raw_json(agent, this, args));
    }
    if intrinsics.get(JSON_IS_RAW_JSON).as_ref() == Some(callee) {
        return Some(is_raw_json_method(agent, this, args));
    }
    None
}
