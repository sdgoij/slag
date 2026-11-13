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

/// A bound on JSON nesting. The recursive parser and serializer otherwise
/// overflow the native stack on adversarial input, which aborts the host.
const MAX_JSON_DEPTH: u32 = 1000;

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
/// ASCII grammar units: JSON grammar tokens are ASCII, and a u16 unit equals
/// its byte value, so the UTF-16 parser's patterns reference these consts
/// (patterns cannot apply `as` casts).
const U_QUOTE: u16 = b'"' as u16;
const U_LBRACE: u16 = b'{' as u16;
const U_RBRACE: u16 = b'}' as u16;
const U_LBRACK: u16 = b'[' as u16;
const U_RBRACK: u16 = b']' as u16;
const U_BSLASH: u16 = b'\\' as u16;
const U_SLASH: u16 = b'/' as u16;
const U_HEX_A: u16 = b'a' as u16;
const U_HEX_F: u16 = b'f' as u16;
const U_HEX_ACAP: u16 = b'A' as u16;
const U_HEX_FCAP: u16 = b'F' as u16;
const U_BELL_B: u16 = b'b' as u16;
const U_U: u16 = b'u' as u16;
const U_COLON: u16 = b':' as u16;
const U_COMMA: u16 = b',' as u16;
const U_MINUS: u16 = b'-' as u16;
const U_PLUS: u16 = b'+' as u16;
const U_DOT: u16 = b'.' as u16;
const U_ZERO: u16 = b'0' as u16;
const U_NINE: u16 = b'9' as u16;
const U_E: u16 = b'e' as u16;
const U_ECAP: u16 = b'E' as u16;
const U_TRUE_T: u16 = b't' as u16;
const U_FALSE_F: u16 = b'f' as u16;
const U_NULL_N: u16 = b'n' as u16;
const U_NL: u16 = b'\n' as u16;
const U_CR: u16 = b'\r' as u16;
const U_TAB: u16 = b'\t' as u16;
const U_LET_R: u16 = b'r' as u16;

struct JsonParser<'a> {
    agent: &'a mut Agent,
    /// The input text as UTF-16 units: parsing natively in UTF-16 avoids the
    /// per-call lossy UTF-8 conversion AND preserves lone surrogates verbatim
    /// (the lossy path replaced them with U+FFFD). ASCII grammar bytes are
    /// their own unit values, so the byte matching carries over unchanged.
    text: &'a [u16],
    pos: usize,
    /// Whether to build the `ParseRecord` tree (sources, entry/element
    /// lists) the reviver needs. A no-reviver parse (the common case)
    /// discards the whole tree, so the scaffolding is skipped entirely:
    /// primitives record no source, objects collect no entries, arrays
    /// collect no element records.
    records: bool,
    /// Current JSONValue nesting, bounded by [`MAX_JSON_DEPTH`].
    depth: u32,
}

impl<'a> JsonParser<'a> {
    fn syntax_error(&self) -> JsError {
        JsError::new(
            ErrorKind::SyntaxError,
            format!("Unexpected token at position {}", self.pos),
        )
    }

    fn skip_ws(&mut self) {
        while let Some(&unit) = self.text.get(self.pos) {
            if matches!(
                unit,
                0x20 | 0x09 | 0x0A | 0x0D // space, tab, LF, CR
            ) {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, unit: u16) -> bool {
        if self.text.get(self.pos) == Some(&unit) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// The ASCII keyword `word` (`true`/`false`/`null`) at the current
    /// position. The units of an ASCII word are its byte values.
    fn eat_word(&mut self, word: &[u8]) -> bool {
        let word_units: [u16; 5] = {
            let mut units = [0u16; 5];
            for (index, &byte) in word.iter().enumerate() {
                units[index] = byte as u16;
            }
            units
        };
        let end = self.pos + word.len();
        if end <= self.text.len() && self.text[self.pos..end] == word_units[..word.len()] {
            self.pos = end;
            true
        } else {
            false
        }
    }

    fn object_proto(&self) -> Option<Handle<JsObject>> {
        // The cached %Object.prototype% accessor: `intrinsics.get` builds a
        // fresh name string + HashMap probe per container, and every JSON
        // object/array create pays it.
        self.agent
            .current_realm()
            .ok()
            .and_then(|realm| realm.intrinsics.object_prototype())
            .and_then(|value| as_object(&value))
    }

    /// Parse a JSONValue, bounding the nesting depth.
    fn parse_value(&mut self) -> Result<ParseRecord, JsError> {
        if self.depth >= MAX_JSON_DEPTH || !crux::stack::has_headroom() {
            return Err(JsError::new(
                ErrorKind::SyntaxError,
                "JSON nesting is too deep".into(),
            ));
        }
        self.depth += 1;
        let result = self.parse_value_inner();
        self.depth -= 1;
        result
    }

    /// Parse a JSONValue; primitives record the raw source range so the
    /// reviver context can reproduce it.
    fn parse_value_inner(&mut self) -> Result<ParseRecord, JsError> {
        self.skip_ws();
        let start = self.pos;
        let value = match self.text.get(self.pos).copied() {
            Some(U_QUOTE) => Value::String(Handle::new(self.parse_string()?)),
            Some(U_LBRACE) => return self.parse_object_record(),
            Some(U_LBRACK) => return self.parse_array_record(),
            Some(U_TRUE_T) if self.eat_word(b"true") => Value::Boolean(true),
            Some(U_FALSE_F) if self.eat_word(b"false") => Value::Boolean(false),
            Some(U_NULL_N) if self.eat_word(b"null") => Value::Null,
            Some(U_MINUS) | Some(U_ZERO..=U_NINE) => Value::Number(self.parse_number()?),
            _ => return Err(self.syntax_error()),
        };
        let end = self.pos;
        let source = if self.records {
            match value.kind() {
                ValueKind::Object(_) | ValueKind::Function(_) => None,
                // The raw token as UTF-16 units, verbatim (no lossy decode).
                _ => Some(JsString::from_utf16(&self.text[start..end])),
            }
        } else {
            None
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
        self.eat(U_LBRACE);
        let object = JsObject::ordinary_object_create(self.object_proto());
        let mut entries = Vec::new();
        self.skip_ws();
        if self.eat(U_RBRACE) {
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
            if !self.eat(U_QUOTE) {
                return Err(self.syntax_error());
            }
            self.pos -= 1;
            let key = self.parse_string()?;
            self.skip_ws();
            if !self.eat(U_COLON) {
                return Err(self.syntax_error());
            }
            let value = self.parse_value()?;
            object.create_data_property_or_throw(&key, value.value)?;
            if self.records {
                entries.push((key, value));
            } else {
                let _ = value;
            }
            self.skip_ws();
            if self.eat(U_RBRACE) {
                break;
            }
            if !self.eat(U_COMMA) {
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

    /// `[ elements ]`: JSONArray with the element records kept for the
    /// reviver. Elements are defined densely on the pre-sized array (no
    /// per-element index-string key, mirroring `array_from_values`).
    fn parse_array_record(&mut self) -> Result<ParseRecord, JsError> {
        self.eat(U_LBRACK);
        let mut elements: Vec<ParseRecord> = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        self.skip_ws();
        if self.eat(U_RBRACK) {
            let array = crate::builtins::array::array_create(self.agent, 0.0)?;
            return Ok(ParseRecord {
                value: Value::Object(array),
                source: None,
                elements,
                entries: Vec::new(),
            });
        }
        loop {
            let record = self.parse_value()?;
            let value = record.value;
            if self.records {
                elements.push(record);
            }
            values.push(value);
            self.skip_ws();
            if self.eat(U_RBRACK) {
                break;
            }
            if !self.eat(U_COMMA) {
                return Err(self.syntax_error());
            }
        }
        let array = crate::builtins::array::array_create(self.agent, values.len() as f64)?;
        for (index, value) in values.iter().enumerate() {
            array.create_data_property_index(index as u64, *value)?;
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
        if !self.eat(U_QUOTE) {
            return Err(self.syntax_error());
        }
        // Fast path: a segment with no escape and no control unit (all
        // illegal unescaped in JSON strings) is the unit range verbatim — one
        // `from_utf16`, no per-unit loop, no Vec. Non-ASCII units are legal
        // unescaped JSON string content (a raw astral char is already a
        // surrogate pair; a raw lone surrogate survives), and `from_utf16`
        // copies them verbatim, so the scan breaks only on the quote, a
        // backslash (an escape follows), or a control unit. Keys and typical
        // string values have no escapes, so this is the common shape.
        let start = self.pos;
        let mut scan = self.pos;
        while let Some(&unit) = self.text.get(scan) {
            if unit == U_QUOTE || unit == U_BSLASH || unit <= 0x1F {
                break;
            }
            scan += 1;
        }
        if self.text.get(scan) == Some(&U_QUOTE) {
            self.pos = scan + 1;
            return Ok(JsString::from_utf16(&self.text[start..scan]));
        }
        // Slow path: an escape, a control unit (illegal unescaped), or an
        // unterminated string.
        let mut units: Vec<u16> = Vec::new();
        loop {
            let Some(&unit) = self.text.get(self.pos) else {
                return Err(self.syntax_error());
            };
            self.pos += 1;
            match unit {
                U_QUOTE => break,
                U_BSLASH => {
                    let Some(&escape) = self.text.get(self.pos) else {
                        return Err(self.syntax_error());
                    };
                    self.pos += 1;
                    match escape {
                        U_QUOTE => units.push(U_QUOTE),
                        U_BSLASH => units.push(U_BSLASH),
                        U_SLASH => units.push(U_SLASH),
                        U_BELL_B => units.push(0x08),
                        U_FALSE_F => units.push(0x0C),
                        // The escape LETTERS n/r/t (0x6E/0x72/0x74); the values
                        // pushed are the control units (0x0A/0x0D/0x09).
                        U_NULL_N => units.push(U_NL),
                        U_LET_R => units.push(U_CR),
                        U_TRUE_T => units.push(U_TAB),
                        U_U => {
                            let hi = self.parse_hex4()?;
                            units.push(hi);
                            if (0xD800..=0xDBFF).contains(&hi)
                                && self.text.get(self.pos..self.pos + 2) == Some(&[U_BSLASH, U_U])
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
                _ => units.push(unit),
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
            let Some(&unit) = self.text.get(self.pos) else {
                return Err(self.syntax_error());
            };
            self.pos += 1;
            let digit = match unit {
                U_ZERO..=U_NINE => unit - U_ZERO,
                U_HEX_A..=U_HEX_F => unit - U_HEX_A + 10,
                U_HEX_ACAP..=U_HEX_FCAP => unit - U_HEX_ACAP + 10,
                _ => return Err(self.syntax_error()),
            };
            value = value * 16 + digit;
        }
        Ok(value)
    }

    /// JSONNumber: `-? int frac? exp?` with no leading zeros and no `+`.
    fn parse_number(&mut self) -> Result<f64, JsError> {
        let start = self.pos;
        self.eat(U_MINUS);
        match self.text.get(self.pos).copied() {
            Some(U_ZERO) => {
                self.pos += 1;
            }
            Some(U_ZERO..=U_NINE) => {
                // The first digit 1-9 (U_ZERO handled above); the range covers
                // it, matching the JSON number grammar's no-leading-zero rule
                // via the U_ZERO arm taking precedence.
                while matches!(self.text.get(self.pos).copied(), Some(U_ZERO..=U_NINE)) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.syntax_error()),
        }
        if self.eat(U_DOT) {
            let frac_start = self.pos;
            while matches!(self.text.get(self.pos).copied(), Some(U_ZERO..=U_NINE)) {
                self.pos += 1;
            }
            if self.pos == frac_start {
                return Err(self.syntax_error());
            }
        }
        if matches!(self.text.get(self.pos).copied(), Some(U_E | U_ECAP)) {
            self.pos += 1;
            if matches!(self.text.get(self.pos).copied(), Some(U_PLUS | U_MINUS)) {
                self.pos += 1;
            }
            let exp_start = self.pos;
            while matches!(self.text.get(self.pos).copied(), Some(U_ZERO..=U_NINE)) {
                self.pos += 1;
            }
            if self.pos == exp_start {
                return Err(self.syntax_error());
            }
        }
        // The JSON number grammar is a subset of the JS numeric-literal
        // grammar. An integer literal (no fraction/exponent) with at most 15
        // digits is EXACTLY representable in an f64, so parse it directly
        // from the units — JSON number values (ids, counts, indices) skip the
        // token-string build + box + generic string→number dispatch the
        // shared conversion pays per value. Longer integers and any
        // fraction/exponent form need correct rounding and fall through to
        // the generic conversion.
        let token = &self.text[start..self.pos];
        let sign = usize::from(token.first() == Some(&U_MINUS));
        if !token.contains(&U_DOT)
            && !token.contains(&U_E)
            && !token.contains(&U_ECAP)
            && token.len() - sign <= 15
        {
            let mut value: u64 = 0;
            for &unit in &token[sign..] {
                value = value * 10 + (unit - U_ZERO) as u64;
            }
            let number = value as f64;
            return Ok(if sign == 1 { -number } else { number });
        }
        let token = JsString::from_utf16(token);
        to_number(&Value::String(Handle::new(token))).map_err(|_| self.syntax_error())
    }
}

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

fn str(text: &str) -> Value {
    Value::String(Handle::new(JsString::from_utf8(text)))
}

/// A serialized property key as a String value (spec Str/SerializeJSONProperty
/// calls the replacer/toJSON with the key).
fn strv(s: &JsString) -> Value {
    Value::String(Handle::new(s.clone()))
}

/// IsRawJSON (spec 26.6.4): an object registered in the raw-JSON table.
fn is_raw_json(agent: &Agent, value: &Value) -> bool {
    match value.kind() {
        ValueKind::Object(obj) => agent.has_raw_json(obj.id()),
        _ => false,
    }
}

/// The [[RawJSON]] text of a raw-JSON object.
fn raw_json_source(agent: &Agent, value: &Value) -> Option<JsString> {
    match value.kind() {
        ValueKind::Object(obj) => agent.raw_json_text(obj.id()),
        _ => None,
    }
}

/// StringToJSONPrimitive (spec 26.6.3.1): the text is exactly one JSON
/// primitive (string, number, boolean, or null); `None` otherwise.
fn json_primitive_value(agent: &mut Agent, text: &JsString) -> Result<Option<Value>, JsError> {
    let units = text.as_slice();
    let mut parser = JsonParser {
        agent,
        text: units,
        pos: 0,
        // A primitive-only parse; the record tree is unused beyond .value.
        records: false,
        depth: 0,
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
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut parser = JsonParser {
        agent,
        text: &units,
        pos: 0,
        // Validation only; the parsed value (and its record tree) is discarded.
        records: false,
        depth: 0,
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
    let reviver = args.get(1).cloned().unwrap_or(Value::Undefined);
    let units = text.as_slice();
    let mut parser = JsonParser {
        agent,
        text: units,
        pos: 0,
        records: is_callable(&reviver),
        depth: 0,
    };
    let record = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos != parser.text.len() {
        return Err(parser.syntax_error());
    }
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
    /// Current object/array nesting, bounded by [`MAX_JSON_DEPTH`].
    depth: u32,
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
        depth: 0,
    };
    // spec 26.6.2 steps 4-5 put the value on a fresh ordinary wrapper and start
    // SerializeJSONProperty at `Get(wrapper, "")`. The wrapper is observable
    // only as the replacer function's `this` (step 3 of 26.6.3.2 calls it with
    // the holder), so the common case — no replacer — passes the root value
    // straight in. The per-call object, its property vector, the empty-key
    // lookup and the `%Object.prototype%` intrinsic read were about half of
    // `JSON.stringify({})`'s 430 ns (`scratch/iso/j0`).
    let empty = JsString::from_utf8("");
    let mut out: Vec<u16> = Vec::new();
    let wrote = if state.replacer_function.is_some() {
        let wrapper = JsObject::ordinary_object_create(
            agent
                .current_realm()?
                .intrinsics
                .get("%Object.prototype%")
                .and_then(|value| as_object(&value)),
        );
        wrapper.create_data_property_or_throw(&empty, value)?;
        serialize_json_property(agent, &mut state, &empty, &Value::Object(wrapper), &mut out)?
    } else {
        serialize_json_value(agent, &mut state, &empty, value, value, &mut out)?
    };
    // The result is assembled in one buffer, so the whole document costs one
    // allocation here instead of a box per member.
    Ok(if wrote {
        Value::String(Handle::new(JsString::from_utf16(&out)))
    } else {
        Value::Undefined
    })
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

/// SerializeJSONProperty (spec 26.6.3.2), appending the member's text to
/// `out`. `Ok(false)` means the value is omitted (undefined, a callable, a
/// symbol) and nothing was appended.
fn serialize_json_property(
    agent: &mut Agent,
    state: &mut StringifyState,
    key: &JsString,
    holder: &Value,
    out: &mut Vec<u16>,
) -> Result<bool, JsError> {
    let value = get_property(agent, holder, key, *holder)?;
    serialize_json_value(agent, state, key, value, *holder, out)
}

/// [`serialize_json_property`] with the value already in hand: the root call
/// has no holder to read it from (see `json_stringify`), and every other caller
/// is a property of a container. `receiver` is the `this` a replacer function
/// is called with — the holder for a property, the (unobservable) root value
/// when the root has no replacer to receive it.
///
/// The text goes straight into the caller's UTF-16 buffer: a primitive is
/// spelled in place (`quote_into`, `units_into`) and a container appends its
/// own framing, so a member costs no intermediate `JsString` at all.
fn serialize_json_value(
    agent: &mut Agent,
    state: &mut StringifyState,
    key: &JsString,
    mut value: Value,
    receiver: Value,
    out: &mut Vec<u16>,
) -> Result<bool, JsError> {
    if matches!(
        value.kind(),
        ValueKind::Object(_) | ValueKind::Function(_) | ValueKind::BigInt(_)
    ) {
        let to_json = get_property(agent, &value, &JsString::from_utf8("toJSON"), value)?;
        if is_callable(&to_json) {
            value = crate::function::call(agent, &to_json, value, &[strv(key)])?;
        }
    }
    if let Some(replacer_function) = &state.replacer_function {
        value = crate::function::call(agent, replacer_function, receiver, &[strv(key), value])?;
    }
    if let ValueKind::Object(obj) = value.kind() {
        if is_raw_json(agent, &value) {
            let source = raw_json_source(agent, &value).unwrap_or_else(|| JsString::from_utf8(""));
            out.extend_from_slice(source.as_slice());
            return Ok(true);
        }
        // Unbox the Number/String/Boolean/BigInt wrappers (spec 26.6.3.2
        // steps 4.a-d). Number and String wrappers convert through the
        // agent (ToNumber/ToString honor overridden valueOf/toString);
        // Boolean/BigInt wrappers serialize their stored primitive. A program
        // that never made a Number/Boolean wrapper has an empty table, and an
        // empty id map is a length check — cheaper than the probe.
        if !agent.number_data.is_empty() && agent.number_data.contains_key(&obj.id()) {
            value = Value::Number(crate::context::to_number(agent, &value)?);
        } else if !agent.boolean_data.is_empty()
            && let Some(b) = agent.boolean_data.get(&obj.id())
        {
            value = Value::Boolean(*b);
        } else if matches!(obj.kind, crux::object::ObjectKind::String(_)) {
            value = Value::String(Handle::new(crate::context::to_string(agent, &value)?));
        } else if let Some(big) = agent.bigint(obj.id()) {
            value = Value::BigInt(Handle::new(big));
        }
    }
    let null = [b'n' as u16, b'u' as u16, b'l' as u16, b'l' as u16];
    match value.kind() {
        ValueKind::Null => out.extend_from_slice(&null),
        ValueKind::Boolean(true) => {
            out.extend_from_slice(&[b't' as u16, b'r' as u16, b'u' as u16, b'e' as u16]);
        }
        ValueKind::Boolean(false) => out.extend_from_slice(&[
            b'f' as u16,
            b'a' as u16,
            b'l' as u16,
            b's' as u16,
            b'e' as u16,
        ]),
        ValueKind::String(s) => quote_into(out, &s),
        ValueKind::Number(n) => {
            if !n.is_finite() {
                out.extend_from_slice(&null);
            } else {
                // The digits are spelled into the result buffer: the boxed
                // form would be copied out of its fresh box immediately.
                let mut buf = [0u16; crux::number::SPELLING_CAP];
                match crux::number::units_into(n, &mut buf) {
                    Some(len) => out.extend_from_slice(&buf[..len]),
                    None => out.extend_from_slice(crux::number::to_string(n).as_slice()),
                }
            }
        }
        ValueKind::BigInt(_) => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Do not know how to serialize a BigInt".into(),
            ));
        }
        // Only non-callable objects are serialized; callable objects,
        // undefined, and symbols are omitted (spec steps 15-16).
        ValueKind::Object(_) | ValueKind::Function(_) if !is_callable(&value) => {
            if crate::builtins::array::is_array(&value) {
                serialize_json_array(agent, state, &value, out)?;
            } else {
                serialize_json_object(agent, state, &value, out)?;
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// SerializeJSONObject (spec 26.6.3.4), bounding the nesting depth.
fn serialize_json_object(
    agent: &mut Agent,
    state: &mut StringifyState,
    value: &Value,
    out: &mut Vec<u16>,
) -> Result<(), JsError> {
    if state.depth >= MAX_JSON_DEPTH || !crux::stack::has_headroom() {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "JSON nesting is too deep".into(),
        ));
    }
    state.depth += 1;
    let result = serialize_json_object_inner(agent, state, value, out);
    state.depth -= 1;
    result
}

fn serialize_json_object_inner(
    agent: &mut Agent,
    state: &mut StringifyState,
    value: &Value,
    out: &mut Vec<u16>,
) -> Result<(), JsError> {
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
    // Assemble the member text directly in the caller's UTF-16 buffer, so a
    // member (its quoted key, its separator and its value) is appended in
    // place instead of being built as its own string.
    if keys.is_empty() {
        // spec 26.6.3.4 step 5: an empty object is "{}" even with a gap.
        out.extend_from_slice(&[b'{' as u16, b'}' as u16]);
        state.indent = stepback;
        state.stack.pop();
        return Ok(());
    }
    let gapped = !state.gap.is_empty();
    let (inner, step) = if gapped {
        (
            state.indent.encode_utf16().collect::<Vec<u16>>(),
            stepback.encode_utf16().collect::<Vec<u16>>(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    out.push(b'{' as u16);
    let open = out.len() - 1;
    let mut wrote = false;
    for key in &keys {
        let mark = out.len();
        if wrote {
            out.push(b',' as u16);
        }
        if gapped {
            out.push(b'\n' as u16);
            out.extend_from_slice(&inner);
        }
        quote_into(out, key);
        out.push(b':' as u16);
        if gapped {
            out.push(b' ' as u16);
        }
        if serialize_json_property(agent, state, key, value, out)? {
            wrote = true;
        } else {
            // Omitted: nothing of this member may reach the result, separator
            // included.
            out.truncate(mark);
        }
    }
    if gapped {
        out.push(b'\n' as u16);
        out.extend_from_slice(&step);
    }
    out.push(b'}' as u16);
    if !wrote {
        // Every member was omitted (a gap would otherwise leave "{\n}"):
        // spec 26.6.3.4 step 5 answers "{}".
        out.truncate(open);
        out.extend_from_slice(&[b'{' as u16, b'}' as u16]);
    }
    state.indent = stepback;
    state.stack.pop();
    Ok(())
}

/// SerializeJSONArray (spec 26.6.3.5), bounding the nesting depth.
fn serialize_json_array(
    agent: &mut Agent,
    state: &mut StringifyState,
    value: &Value,
    out: &mut Vec<u16>,
) -> Result<(), JsError> {
    if state.depth >= MAX_JSON_DEPTH || !crux::stack::has_headroom() {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "JSON nesting is too deep".into(),
        ));
    }
    state.depth += 1;
    let result = serialize_json_array_inner(agent, state, value, out);
    state.depth -= 1;
    result
}

fn serialize_json_array_inner(
    agent: &mut Agent,
    state: &mut StringifyState,
    value: &Value,
    out: &mut Vec<u16>,
) -> Result<(), JsError> {
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
    if length == 0 {
        // spec 26.6.3.5 step 4: an empty array serializes to "[]" even with
        // a gap (the object path has the same empty special-case).
        out.extend_from_slice(&[b'[' as u16, b']' as u16]);
        state.indent = stepback;
        state.stack.pop();
        return Ok(());
    }
    let gapped = !state.gap.is_empty();
    let (inner, step) = if gapped {
        (
            state.indent.encode_utf16().collect::<Vec<u16>>(),
            stepback.encode_utf16().collect::<Vec<u16>>(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    out.push(b'[' as u16);
    for index in 0..length {
        if index > 0 {
            out.push(b',' as u16);
        }
        if gapped {
            out.push(b'\n' as u16);
            out.extend_from_slice(&inner);
        }
        let key = JsString::from_utf8(&index.to_string());
        // spec 26.6.3.5 step 8: an omitted element serializes as "null".
        if !serialize_json_property(agent, state, &key, value, out)? {
            out.extend_from_slice(&[b'n' as u16, b'u' as u16, b'l' as u16, b'l' as u16]);
        }
    }
    if gapped {
        out.push(b'\n' as u16);
        out.extend_from_slice(&step);
    }
    out.push(b']' as u16);
    state.indent = stepback;
    state.stack.pop();
    Ok(())
}

/// QuoteJSONString (spec 26.6.3.6): escape quotes, backslashes, control
/// characters, and lone surrogates, APPENDING the quoted form to `out` (the
/// serializer assembles member text directly in UTF-16 units).
fn quote_into(out: &mut Vec<u16>, s: &JsString) {
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
    agent.register_raw_json(object.id(), text.clone());
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

/// The O(1) dispatch table for the JSON members that need the agent:
/// `Intrinsics::define` registers each builtin function's id against its
/// native handler here, so a warm call skips the intrinsic-identity probes in
/// [`dispatch_call`] (which run `current_realm` + `intrinsics.get` with a
/// fresh name string per call).
pub(crate) fn handler_for(name: &str) -> Option<crate::function::BuiltinHandler> {
    match name {
        JSON_PARSE => Some(json_parse),
        JSON_STRINGIFY => Some(json_stringify),
        JSON_RAW_JSON => Some(raw_json),
        JSON_IS_RAW_JSON => Some(is_raw_json_method),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    fn run(source: &str) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm()?;
        agent.run_script(source)
    }

    /// The serializer appends into one buffer, so an omitted member (and the
    /// separator that would have carried it) must leave nothing behind —
    /// including the newline a gap would otherwise emit for an object whose
    /// every member is omitted.
    #[test]
    fn stringify_leaves_no_trace_of_an_omitted_member() {
        let cases = [
            ("JSON.stringify({x: undefined}, null, 2)", "{}"),
            (
                "JSON.stringify({x: undefined, y: undefined}, null, 2)",
                "{}",
            ),
            (
                "JSON.stringify({x: undefined, y: 1}, null, 2)",
                "{\n  \"y\": 1\n}",
            ),
            (
                "JSON.stringify({x: 1, y: undefined}, null, 2)",
                "{\n  \"x\": 1\n}",
            ),
            (
                "JSON.stringify({a: {b: undefined}}, null, 1)",
                "{\n \"a\": {}\n}",
            ),
            (
                "JSON.stringify([undefined, 1], null, 2)",
                "[\n  null,\n  1\n]",
            ),
            (
                "JSON.stringify({a: [1, {c: undefined}]}, null, 1)",
                "{\n \"a\": [\n  1,\n  {}\n ]\n}",
            ),
        ];
        for (source, expected) in cases {
            let value = run(source).expect("evaluates");
            let text = value.as_string().expect("a string");
            let want: Vec<u16> = expected.encode_utf16().collect();
            assert_eq!(
                text.as_slice(),
                want.as_slice(),
                "{source} should be {expected:?}"
            );
        }
    }

    #[test]
    fn parse_and_stringify_bound_their_nesting() {
        assert!(matches!(
            run("JSON.parse('['.repeat(2000)+']'.repeat(2000))"),
            Err(e) if e.kind == ErrorKind::SyntaxError
        ));
        assert!(matches!(
            run("let o={},c=o; for(let i=0;i<2000;i++){c.n={};c=c.n} JSON.stringify(o)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }
}
