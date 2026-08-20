//! `Intl.Segmenter` (ECMA-402 §19): the constructor (granularity option,
//! locale resolution), `segment` (the `Segments` object with `containing`
//! and `[Symbol.iterator]`), and the per-call Segment Iterators. The
//! segmentation itself is UAX #29 in the `unicode` crate (plan Cut 7).
//! Instances store their record in the agent's `intl_segmenter_data` map;
//! the `Segments` objects and Segment Iterators carry their internal slots
//! in `intl_segments_data` / `intl_segment_iterator_data`.

use crux::convert::{to_integer_or_infinity, to_number};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::builtins::intl::number_format::{self, get_option};
use crate::context::{as_object, get_property, to_string};
use crate::realm::Realm;

pub const SEGMENTER: &str = "%Intl.Segmenter%";
pub const SEGMENTER_PROTO: &str = "%Intl.Segmenter.prototype%";
pub const SEGMENTER_SEGMENT: &str = "%Intl.Segmenter.prototype.segment%";
pub const SEGMENTER_RESOLVED_OPTIONS: &str = "%Intl.Segmenter.prototype.resolvedOptions%";
pub const SEGMENTER_SUPPORTED_LOCALES_OF: &str = "%Intl.Segmenter.supportedLocalesOf%";
const SEGMENTS_PROTO: &str = "%IntlSegmentsPrototype%";
const SEGMENTS_CONTAINING: &str = "%IntlSegmentsPrototype.containing%";
const SEGMENTS_ITERATOR: &str = "%IntlSegmentsPrototype[Symbol.iterator]%";
const SEGMENT_ITERATOR_PROTO: &str = "%IntlSegmentIteratorPrototype%";
const SEGMENT_ITERATOR_NEXT: &str = "%IntlSegmentIteratorPrototype.next%";

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

/// The [[InitializedSegmenter]] record (ECMA-402 §19.1.1).
#[derive(Debug, Clone)]
pub struct SegmenterRecord {
    pub locale: String,
    pub granularity: String,
}

/// The [[SegmentsSegmenter]]/[[SegmentsString]] slots of a Segments object.
#[derive(Debug, Clone)]
pub struct SegmentsRecord {
    pub segmenter_id: u64,
    pub string: JsString,
}

/// The [[IteratingSegmenter]]/[[IteratedString]]/
/// [[IteratedStringNextSegmentCodeUnitIndex]] slots of a Segment Iterator.
#[derive(Debug, Clone)]
pub struct SegmentIteratorRecord {
    pub segmenter_id: u64,
    pub string: JsString,
    pub next_index: u64,
}

/// Intl.Segmenter (ECMA-402 §19.1.1): locale resolution, then the
/// granularity option. The `lineBreakStyle` option was removed from the
/// spec and is never read (the corpus's `options-order.js` pins that).
fn initialize(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
) -> Result<SegmenterRecord, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, locales)?;
    // GetOptionsObject: undefined → a null-prototype object; any other
    // non-object throws (no ToObject coercion, unlike supportedLocalesOf).
    let options = get_options_object(agent, options)?;
    get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let locale = number_format::resolve_locale_simple(&requested)?;
    let granularity = get_option(
        agent,
        &options,
        "granularity",
        &["grapheme", "word", "sentence"],
        Some("grapheme"),
    )?;
    Ok(SegmenterRecord {
        locale,
        granularity: granularity.unwrap_or_else(|| "grapheme".to_string()),
    })
}

/// GetOptionsObject (ECMA-402 §9.2.10): undefined → a null-prototype
/// object; objects pass through; everything else throws a TypeError.
fn get_options_object(_agent: &mut Agent, options: &Value) -> Result<Value, JsError> {
    if options.is_undefined() {
        Ok(Value::Object(JsObject::ordinary_object_create(None)))
    } else if as_object(options).is_some() {
        Ok(options.clone())
    } else {
        Err(type_error("Options must be an object"))
    }
}

pub fn install(realm: &Handle<Realm>, intl_value: &Value) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    let proto = JsObject::ordinary_object_create(object_proto.clone());
    let ctor = Function::create_builtin(
        Some(JsString::from_utf8("Segmenter")),
        0,
        placeholder("Intl.Segmenter"),
        Some(placeholder("Intl.Segmenter")),
        function_proto.clone(),
    )?;
    proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(Value::Function(ctor.clone())),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let segment = Function::create_builtin(
        Some(JsString::from_utf8("segment")),
        1,
        placeholder("segment"),
        None,
        function_proto.clone(),
    )?;
    realm
        .intrinsics
        .define(SEGMENTER_SEGMENT, Value::Function(segment.clone()));
    proto.define_property(
        &JsString::from_utf8("segment"),
        &PropertyDescriptor {
            value: Some(Value::Function(segment)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let resolved = Function::create_builtin(
        Some(JsString::from_utf8("resolvedOptions")),
        0,
        placeholder("resolvedOptions"),
        None,
        function_proto.clone(),
    )?;
    realm.intrinsics.define(
        SEGMENTER_RESOLVED_OPTIONS,
        Value::Function(resolved.clone()),
    );
    proto.define_property(
        &JsString::from_utf8("resolvedOptions"),
        &PropertyDescriptor {
            value: Some(Value::Function(resolved)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // %Intl.Segmenter.prototype%[@@toStringTag] = "Intl.Segmenter".
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Intl.Segmenter",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let proto_value = Value::Object(proto.clone());
    ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    let supported = Function::create_builtin(
        Some(JsString::from_utf8("supportedLocalesOf")),
        1,
        placeholder("supportedLocalesOf"),
        None,
        function_proto.clone(),
    )?;
    realm.intrinsics.define(
        SEGMENTER_SUPPORTED_LOCALES_OF,
        Value::Function(supported.clone()),
    );
    ctor.define_property(
        &JsString::from_utf8("supportedLocalesOf"),
        &PropertyDescriptor {
            value: Some(Value::Function(supported)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    realm.intrinsics.define(SEGMENTER_PROTO, proto_value);
    realm
        .intrinsics
        .define(SEGMENTER, Value::Function(ctor.clone()));
    if let Some(obj) = as_object(intl_value) {
        obj.define_property(
            &JsString::from_utf8("Segmenter"),
            &PropertyDescriptor {
                value: Some(Value::Function(ctor)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    // %IntlSegmentsPrototype% (ECMA-402 §19.5.2): an ordinary object with
    // %Object.prototype% as its prototype.
    let segments_proto = JsObject::ordinary_object_create(object_proto);
    let containing = Function::create_builtin(
        Some(JsString::from_utf8("containing")),
        1,
        placeholder("containing"),
        None,
        function_proto.clone(),
    )?;
    realm
        .intrinsics
        .define(SEGMENTS_CONTAINING, Value::Function(containing.clone()));
    segments_proto.define_property(
        &JsString::from_utf8("containing"),
        &PropertyDescriptor {
            value: Some(Value::Function(containing)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let segments_iter = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.iterator]")),
        0,
        placeholder("[Symbol.iterator]"),
        None,
        function_proto.clone(),
    )?;
    realm
        .intrinsics
        .define(SEGMENTS_ITERATOR, Value::Function(segments_iter.clone()));
    segments_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(segments_iter)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    realm
        .intrinsics
        .define(SEGMENTS_PROTO, Value::Object(segments_proto));

    // %IntlSegmentIteratorPrototype% (ECMA-402 §19.6.2): an ordinary object
    // whose prototype is %Iterator.prototype%.
    let iterator_proto = realm
        .intrinsics
        .get("%Iterator.prototype%")
        .and_then(|value| as_object(&value));
    let segment_iter_proto = JsObject::ordinary_object_create(iterator_proto);
    let next = Function::create_builtin(
        Some(JsString::from_utf8("next")),
        0,
        placeholder("next"),
        None,
        function_proto.clone(),
    )?;
    realm
        .intrinsics
        .define(SEGMENT_ITERATOR_NEXT, Value::Function(next.clone()));
    segment_iter_proto.define_property(
        &JsString::from_utf8("next"),
        &PropertyDescriptor {
            value: Some(Value::Function(next)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // %IntlSegmentIteratorPrototype%[@@toStringTag] = "Segmenter String Iterator".
    segment_iter_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Segmenter String Iterator",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    realm
        .intrinsics
        .define(SEGMENT_ITERATOR_PROTO, Value::Object(segment_iter_proto));
    Ok(())
}

fn placeholder(name: &str) -> NativeFn {
    let name = name.to_string();
    Box::new(move |_, _| Err(type_error(&format!("{name} must be dispatched"))))
}

/// The record of `this` (RequireInternalSlot).
fn segmenter_record(agent: &Agent, this: &Value) -> Result<SegmenterRecord, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error("Not a Segmenter instance"));
    };
    agent
        .intl_segmenter_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Not a Segmenter instance"))
}

/// The record of the Segmenter behind a Segments object / Segment Iterator.
fn segmenter_record_by_id(agent: &Agent, id: u64) -> Result<SegmenterRecord, JsError> {
    agent
        .intl_segmenter_data
        .get(&id)
        .cloned()
        .ok_or_else(|| type_error("Not a Segmenter instance"))
}

/// The record of `this` for the prototype members.
fn unwrap_segmenter(agent: &mut Agent, value: &Value) -> Result<Value, JsError> {
    let Some(obj) = as_object(value) else {
        return Err(type_error("Not a Segmenter instance"));
    };
    if agent.intl_segmenter_data.contains_key(&obj.id()) {
        return Ok(value.clone());
    }
    Err(type_error("Not a Segmenter instance"))
}

/// GetPrototypeFromConstructor: the newTarget's `prototype`, falling back to
/// %Intl.Segmenter.prototype% of the newTarget's realm.
fn proto_from_ctor(agent: &mut Agent, new_target: &Value) -> Result<Handle<JsObject>, JsError> {
    let proto = get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        new_target.clone(),
    )?;
    if let Some(obj) = as_object(&proto) {
        return Ok(obj);
    }
    crate::context::get_function_realm(agent, new_target)?
        .intrinsics
        .get(SEGMENTER_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| type_error("%Intl.Segmenter.prototype% missing"))
}

fn create_instance(
    agent: &mut Agent,
    proto: Handle<JsObject>,
    record: SegmenterRecord,
) -> Result<Value, JsError> {
    let instance = JsObject::ordinary_object_create(Some(proto));
    agent.intl_segmenter_data.insert(instance.id(), record);
    Ok(Value::Object(instance))
}

/// Intl.Segmenter.prototype.segment (ECMA-402 §19.4.2): a fresh Segments
/// object for the ToString'd argument.
fn segment_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let segmenter = unwrap_segmenter(agent, this)?;
    let string = to_string(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    create_segments_object(agent, &segmenter, string)
}

/// CreateSegmentsObject (ECMA-402 §19.5.1).
fn create_segments_object(
    agent: &mut Agent,
    segmenter: &Value,
    string: JsString,
) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let proto = realm
        .intrinsics
        .get(SEGMENTS_PROTO)
        .and_then(|value| as_object(&value));
    let object = JsObject::ordinary_object_create(proto);
    let segmenter_id = as_object(segmenter)
        .ok_or_else(|| type_error("Not a Segmenter instance"))?
        .id();
    agent.intl_segments_data.insert(
        object.id(),
        SegmentsRecord {
            segmenter_id,
            string,
        },
    );
    Ok(Value::Object(object))
}

/// The [[SegmentsSegmenter]]/[[SegmentsString]] slots of `this`.
fn require_segments(agent: &Agent, this: &Value) -> Result<SegmentsRecord, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error("Not a Segments object"));
    };
    agent
        .intl_segments_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Not a Segments object"))
}

/// %IntlSegmentsPrototype%.containing (ECMA-402 §19.5.2.1).
fn containing(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let segments = require_segments(agent, this)?;
    let record = segmenter_record_by_id(agent, segments.segmenter_id)?;
    let string = segments.string;
    let length = string.len();
    let n = to_integer_or_infinity(to_number(
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    if n < 0.0 || n >= length as f64 {
        return Ok(Value::Undefined);
    }
    let n = n as usize;
    let boundaries = segment_boundaries(&record, &string);
    let start = find_boundary(&boundaries, n, true);
    let end = find_boundary(&boundaries, n, false);
    create_segment_data_object(agent, &record, &string, start, end)
}

/// %IntlSegmentsPrototype%[@@iterator] (ECMA-402 §19.5.2.2): a fresh
/// Segment Iterator per call — independent iterators share nothing.
fn segments_iterator(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let segments = require_segments(agent, this)?;
    create_segment_iterator(agent, segments.segmenter_id, segments.string)
}

/// CreateSegmentIterator (ECMA-402 §19.6.1).
fn create_segment_iterator(
    agent: &mut Agent,
    segmenter_id: u64,
    string: JsString,
) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let proto = realm
        .intrinsics
        .get(SEGMENT_ITERATOR_PROTO)
        .and_then(|value| as_object(&value));
    let object = JsObject::ordinary_object_create(proto);
    agent.intl_segment_iterator_data.insert(
        object.id(),
        SegmentIteratorRecord {
            segmenter_id,
            string,
            next_index: 0,
        },
    );
    Ok(Value::Object(object))
}

/// %IntlSegmentIteratorPrototype%.next (ECMA-402 §19.6.2.1).
fn segment_iterator_next(
    agent: &mut Agent,
    this: &Value,
    _args: &[Value],
) -> Result<Value, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error("Not a Segment Iterator"));
    };
    let mut record = agent
        .intl_segment_iterator_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Not a Segment Iterator"))?;
    let segmenter_record = segmenter_record_by_id(agent, record.segmenter_id)?;
    let string = record.string.clone();
    let length = string.len();
    let start_index = record.next_index as usize;
    if start_index >= length {
        return iterator_result(Value::Undefined, true);
    }
    let boundaries = segment_boundaries(&segmenter_record, &string);
    let end_index = find_boundary(&boundaries, start_index, false);
    record.next_index = end_index as u64;
    agent.intl_segment_iterator_data.insert(obj.id(), record);
    let segment_data =
        create_segment_data_object(agent, &segmenter_record, &string, start_index, end_index)?;
    iterator_result(segment_data, false)
}

/// CreateIteratorResultObject (spec 7.4.9).
fn iterator_result(value: Value, done: bool) -> Result<Value, JsError> {
    let object = JsObject::ordinary_object_create(None);
    object.create_data_property(&JsString::from_utf8("value"), value)?;
    object.create_data_property(&JsString::from_utf8("done"), Value::Boolean(done))?;
    Ok(Value::Object(object))
}

/// CreateSegmentDataObject (ECMA-402 §19.7.1): `segment`/`index`/`input`,
/// plus `isWordLike` for the word granularity.
fn create_segment_data_object(
    agent: &mut Agent,
    record: &SegmenterRecord,
    string: &JsString,
    start: usize,
    end: usize,
) -> Result<Value, JsError> {
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let object = JsObject::ordinary_object_create(object_proto);
    let define = |name: &str, value: Value| -> Result<(), JsError> {
        object.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(value),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(true),
            },
        )?;
        Ok(())
    };
    define(
        "segment",
        Value::String(Handle::new(substring(string, start, end))),
    )?;
    define("index", Value::Number(start as f64))?;
    define("input", Value::String(Handle::new(string.clone())))?;
    if record.granularity == "word" {
        let word_like = segment_is_word_like(string, start, end);
        define("isWordLike", Value::Boolean(word_like))?;
    }
    Ok(Value::Object(object))
}

/// The UTF-16 code-unit segment boundaries of `string` for the record's
/// granularity (always including 0 and the length).
fn segment_boundaries(record: &SegmenterRecord, string: &JsString) -> Vec<usize> {
    let granularity = match record.granularity.as_str() {
        "word" => unicode::SegmentationGranularity::Word,
        "sentence" => unicode::SegmentationGranularity::Sentence,
        _ => unicode::SegmentationGranularity::Grapheme,
    };
    let cps = string.to_code_points();
    let cp_boundaries = unicode::segment_boundaries(&cps, granularity);
    // Map code-point offsets to UTF-16 code-unit offsets.
    let mut cu_at = vec![0usize; cps.len() + 1];
    let mut offset = 0usize;
    for (i, &cp) in cps.iter().enumerate() {
        cu_at[i] = offset;
        offset += if cp > 0xFFFF { 2 } else { 1 };
    }
    cu_at[cps.len()] = offset;
    cp_boundaries.into_iter().map(|b| cu_at[b]).collect()
}

/// FindBoundary (ECMA-402 §19.8.1): the last boundary at or before `n`
/// (`before`) or the first boundary after the code unit at `n` (`after`).
fn find_boundary(boundaries: &[usize], n: usize, before: bool) -> usize {
    if before {
        boundaries
            .iter()
            .rev()
            .find(|&&b| b <= n)
            .copied()
            .unwrap_or(0)
    } else {
        boundaries
            .iter()
            .find(|&&b| b > n)
            .copied()
            .unwrap_or_else(|| boundaries.last().copied().unwrap_or(0))
    }
}

fn substring(s: &JsString, from: usize, to: usize) -> JsString {
    let len = s.len();
    let from = from.min(len);
    let to = to.min(len).max(from);
    JsString::from_utf16(&s.as_slice()[from..to])
}

/// Whether the segment `[start, end)` is word-like: it contains a letter or
/// digit (the same predicate the unicode crate uses to build word segments).
fn segment_is_word_like(string: &JsString, start: usize, end: usize) -> bool {
    let units = &string.as_slice()[start..end];
    let mut i = 0;
    while i < units.len() {
        let cp = if units[i] >= 0xD800
            && units[i] <= 0xDBFF
            && i + 1 < units.len()
            && (0xDC00..=0xDFFF).contains(&units[i + 1])
        {
            let hi = (units[i] - 0xD800) as u32;
            let lo = (units[i + 1] - 0xDC00) as u32;
            i += 2;
            0x10000 + (hi << 10) + lo
        } else {
            let cp = units[i] as u32;
            i += 1;
            cp
        };
        if unicode::is_identifier_part(cp)
            || unicode::binary_property(cp, "Alphabetic") == Some(true)
        {
            return true;
        }
    }
    false
}

/// Intl.Segmenter.prototype.resolvedOptions (ECMA-402 §19.4.3).
fn resolved_options_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let record = segmenter_record(agent, this)?;
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let options = JsObject::ordinary_object_create(object_proto);
    let define = |name: &str, value: Value| -> Result<(), JsError> {
        options.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(value),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(true),
            },
        )?;
        Ok(())
    };
    let str = |s: &str| Value::String(Handle::new(JsString::from_utf8(s)));
    define("locale", str(&record.locale))?;
    define("granularity", str(&record.granularity))?;
    Ok(Value::Object(options))
}

/// Intl.Segmenter.supportedLocalesOf (ECMA-402 §19.2.2).
fn supported_locales_of(
    agent: &mut Agent,
    locales: Value,
    options: Value,
) -> Result<Value, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, &locales)?;
    // SupportedLocales: non-undefined options are coerced with ToObject
    // (unlike the constructor's GetOptionsObject).
    let options = number_format::coerce_options_to_object(agent, &options)?;
    get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let available = crate::builtins::intl::number_data::NUMBER_FORMAT_LOCALES;
    let mut subset = Vec::new();
    for locale in &requested {
        let base = number_format::strip_unicode_extension(locale);
        if number_format::best_fit(available, &base).is_some() {
            subset.push(Value::String(Handle::new(JsString::from_utf8(locale))));
        }
    }
    crate::builtins::array::array_from_values(agent, &subset)
}

/// dispatch_call: the Segmenter constructor (as a function — throws), the
/// prototype members, the Segments members, and supportedLocalesOf.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(SEGMENTER).as_ref() == Some(callee) {
        return Some(Err(type_error("Intl.Segmenter requires 'new'")));
    }
    if intrinsics.get(SEGMENTER_SUPPORTED_LOCALES_OF).as_ref() == Some(callee) {
        return Some(supported_locales_of(
            agent,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if intrinsics.get(SEGMENTER_RESOLVED_OPTIONS).as_ref() == Some(callee) {
        return Some(resolved_options_method(agent, this));
    }
    if intrinsics.get(SEGMENTER_SEGMENT).as_ref() == Some(callee) {
        return Some(segment_method(agent, this, args));
    }
    if intrinsics.get(SEGMENTS_CONTAINING).as_ref() == Some(callee) {
        return Some(containing(agent, this, args));
    }
    if intrinsics.get(SEGMENTS_ITERATOR).as_ref() == Some(callee) {
        return Some(segments_iterator(agent, this, args));
    }
    if intrinsics.get(SEGMENT_ITERATOR_NEXT).as_ref() == Some(callee) {
        return Some(segment_iterator_next(agent, this, args));
    }
    None
}

/// dispatch_construct: `new Intl.Segmenter(...)`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(SEGMENTER).as_ref() == Some(callee) {
        let proto = match proto_from_ctor(agent, new_target) {
            Ok(proto) => proto,
            Err(error) => return Some(Err(error)),
        };
        let locales = args.first().cloned().unwrap_or(Value::Undefined);
        let options = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Some(match initialize(agent, &locales, &options) {
            Ok(record) => create_instance(agent, proto, record),
            Err(error) => Err(error),
        });
    }
    None
}
