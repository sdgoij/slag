//! A heap snapshot in V8's document shape (`v8::Isolate::TakeHeapSnapshot`).
//!
//! A host asks for a `.heapsnapshot` document — a JSON description of every
//! object, its shallow size, and the references between them — to load in
//! DevTools, or (as `ext/node/ops/v8.rs` does) to walk with a JSON parser and
//! count the objects of one constructor. This builds that document from the
//! graph the collector itself marks over: [`crux::heap::Heap::for_each_live_box`]
//! walks every live box and reports the same edges a collection follows, so the
//! snapshot is complete to the engine's own model rather than to a re-walk of
//! the JS-visible graph.
//!
//! # What a node is
//!
//! V8's snapshot is post-GC and reports the reachable objects. This walk reports
//! every *live* box, which is the reachable set after a collection and a superset
//! of it before one (garbage not yet swept is live). The walk does not collect
//! first; a host that wants V8's exact reachable set calls `gc()` before it, which
//! is what Node's own `writeHeapSnapshot` does.
//!
//! `self_size` is the box's arena footprint — payload plus GC header, rounded to
//! the arena's granularity. That keeps V8's meaning (a shallow size, not a
//! retained size), but the unit is *this* arena's, not an object's `sizeof`.
//!
//! # Naming
//!
//! A box the walk can type gets V8's word for it: a `JsObject` box is `object`, a
//! `Function` box is `closure`, and a `JsString`/`Symbol`/`BigInt` box is
//! `string`/`symbol`/`bigint`. Everything else — property maps, slot cells,
//! environments, array elements — is `hidden`, V8's own word for an internal
//! object it shows without a name, named here by the payload's Rust type so the
//! node is still identifiable.
//!
//! An `object` node is named by the constructor its prototype chain reports, the
//! answer `v8::Object::GetConstructorName` gives. The chain is read as data
//! properties only, and a proxy is not entered at all: taking a snapshot must not
//! run host JS (a getter or a proxy trap may allocate, and V8's snapshot runs
//! none either), and it must not grow the heap it is describing. An object whose
//! chain names no constructor falls back to its own kind ("Array", "Proxy", ...)
//! rather than to an invented name. A `closure` node is named by its
//! [`Function::name`](crux::function::Function::name).
//!
//! # Streaming
//!
//! The callback is V8's `HeapSnapshotOutputStream` protocol: the document is
//! written in chunks rather than buffered whole, and a callback that answers
//! `false` (a host's write error) stops the stream. A snapshot is large, so
//! nothing here holds the whole document in one buffer.

use std::collections::HashMap;

use crux::handle::Handle;
use crux::heap::{self, GcAny};
use crux::object::{JsObject, ObjectKind, PropertyKind};
use crux::property::PropertyKey;
use crux::string::JsString;

use crate::Isolate;

/// The numbers the document spends on each node, and the stride `nodes` is read
/// at. V8's `node_fields` length; the two must agree or a parser misreads the
/// array.
const NODE_FIELD_COUNT: u64 = 7;

/// A node's type, as the index into the type row of V8's `node_types`: the
/// position of the word V8 uses, which matters because
/// `op_v8_query_objects_count` matches the `object` word by position.
mod node_type {
    pub const HIDDEN: u64 = 0;
    pub const STRING: u64 = 2;
    pub const OBJECT: u64 = 3;
    pub const CLOSURE: u64 = 5;
    pub const SYMBOL: u64 = 12;
    pub const BIGINT: u64 = 13;
}

/// The edge type every edge here carries. The references between engine objects
/// are internal edges, not JS property or element edges — `trace` reports what
/// the collector follows, which is a level below the language's.
const EDGE_TYPE_INTERNAL: u64 = 3;

/// How far a prototype-chain walk goes before giving up. A spec-conforming chain
/// cannot cycle (`[[SetPrototypeOf]]` refuses a cycle with a TypeError), but a
/// bound keeps a malformed one from becoming a hang.
const MAX_PROTOTYPE_DEPTH: usize = 1024;

/// V8's `snapshot.meta`, verbatim. `node_fields` fixes the stride this document
/// writes, `node_types` carries the `object` word `queryObjects` looks for, and
/// the rest are the trace/sample tables an empty document declares and leaves
/// empty.
const META: &str = concat!(
    r#"{"node_fields":["type","name","id","self_size","edge_count","trace_node_id","detachedness"],"#,
    r#""node_types":[["hidden","array","string","object","code","closure","regexp","number","native","synthetic","concatenated string","sliced string","symbol","bigint","object shape"],"string","number","number","number","number","number"],"#,
    r#""edge_fields":["type","name_or_index","to_node"],"#,
    r#""edge_types":[["context","element","property","internal","hidden","shortcut","weak"],"string_or_number","node"],"#,
    r#""trace_function_info_fields":["function_id","name","script_name","script_id","line","column"],"#,
    r#""trace_node_fields":["id","function_info_index","count","size","children"],"#,
    r#""sample_fields":["timestamp_us","last_assigned_id"],"#,
    r#""location_fields":["object_index","script_id","line","column"]}"#,
);

impl Isolate {
    /// Stream a heap snapshot to `callback` (`v8::Isolate::TakeHeapSnapshot`).
    ///
    /// The callback receives the document in chunks and answers whether to
    /// continue: `false` stops the stream, which is how a host report a write
    /// error (`ext/node/ops/v8.rs`'s `writeHeapSnapshot` is the one caller that
    /// does). The isolate is not consulted — the graph is the arena's, walked
    /// through `crux` — so a snapshot needs no entered context and can be taken
    /// from a GC callback, which is where `setHeapSnapshotNearHeapLimit` wants
    /// one.
    pub fn take_heap_snapshot<F: FnMut(&[u8]) -> bool>(&mut self, callback: F) {
        write_document(callback);
    }
}

/// What the walk learned about one live box, before naming (which reads no arena
/// memory into existence) and before edges are turned into node ordinals.
struct Box {
    any: GcAny,
    /// The payload's Rust type name, for a box the engine has no better word for.
    type_name: &'static str,
    /// The box's arena footprint in bytes.
    size: usize,
    role: Role,
    /// The boxes this one points at, as addresses (mapped to ordinals later).
    edge_addrs: Vec<usize>,
}

/// The V8 word a box is typed as, decided by the concrete payload type.
#[derive(Clone, Copy)]
enum Role {
    Object,
    Closure,
    String,
    Symbol,
    BigInt,
    Hidden,
}

/// One node of the document, ready to serialize.
struct Node {
    type_index: u64,
    /// Index into the string table.
    name: u32,
    size: u64,
    /// The node ordinals this one points at.
    targets: Vec<u32>,
}

fn write_document<F: FnMut(&[u8]) -> bool>(callback: F) {
    // The naming keys are interned before the walk, so no arena allocation can
    // happen between the walk and the naming pass — which is what makes the
    // addresses the walk yielded still valid when naming reads them.
    let naming = Naming::new();
    let boxes = walk();

    let mut ordinal: HashMap<usize, u32> = HashMap::with_capacity(boxes.len());
    for (index, boxed) in boxes.iter().enumerate() {
        ordinal.insert(boxed.any.addr(), index as u32);
    }

    let mut strings = Strings::new();
    let mut nodes: Vec<Node> = Vec::with_capacity(boxes.len());
    let mut edge_count: u64 = 0;
    for boxed in &boxes {
        let (type_index, name) = naming.classify(boxed);
        // An edge to a box the walk did not report would be a dangling index,
        // so it is dropped rather than emitted.
        let targets: Vec<u32> = boxed
            .edge_addrs
            .iter()
            .filter_map(|addr| ordinal.get(addr).copied())
            .collect();
        edge_count += targets.len() as u64;
        nodes.push(Node {
            type_index,
            name: strings.index_of(&name),
            size: boxed.size as u64,
            targets,
        });
    }

    let mut emitter = Emitter::new(callback);
    emitter.push_str("{\"snapshot\":{\"meta\":");
    emitter.push_str(META);
    emitter.push_str(",\"node_count\":");
    emitter.push_u64(nodes.len() as u64);
    emitter.push_str(",\"edge_count\":");
    emitter.push_u64(edge_count);
    emitter.push_str(",\"trace_function_count\":0},\"nodes\":[");
    for (index, node) in nodes.iter().enumerate() {
        if emitter.aborted() {
            break;
        }
        if index > 0 {
            emitter.push_byte(b',');
        }
        emitter.push_u64(node.type_index);
        emitter.push_byte(b',');
        emitter.push_u64(u64::from(node.name));
        // `id` is 1-based and stable within this document; `trace_node_id` and
        // `detachedness` are the zeroes this engine has nothing to put in.
        emitter.push_byte(b',');
        emitter.push_u64(index as u64 + 1);
        emitter.push_byte(b',');
        emitter.push_u64(node.size);
        emitter.push_byte(b',');
        emitter.push_u64(node.targets.len() as u64);
        emitter.push_str(",0,0");
    }
    if !emitter.aborted() {
        emitter.push_str("],\"edges\":[");
        let mut first = true;
        'nodes: for node in &nodes {
            for (position, target) in node.targets.iter().enumerate() {
                if emitter.aborted() {
                    break 'nodes;
                }
                if !first {
                    emitter.push_byte(b',');
                }
                first = false;
                emitter.push_u64(EDGE_TYPE_INTERNAL);
                emitter.push_byte(b',');
                // An internal edge's name_or_index is a number: the edge's
                // position within its source node.
                emitter.push_u64(position as u64);
                emitter.push_byte(b',');
                // `to_node` is the target's offset into the `nodes` array.
                emitter.push_u64(u64::from(*target) * NODE_FIELD_COUNT);
            }
        }
        emitter.push_str("],\"strings\":[");
        for (index, text) in strings.entries().iter().enumerate() {
            if emitter.aborted() {
                break;
            }
            if index > 0 {
                emitter.push_byte(b',');
            }
            emitter.push_json_string(text);
        }
        emitter.push_str(
            "],\"trace_function_infos\":[],\"trace_tree\":[],\"samples\":[],\"locations\":[]}",
        );
    }
    // The final flush is where a small document's only callback happens, so a
    // `false` there is a write error like any other.
    emitter.finish();
}

/// Walk the arena into the boxes this document is made of.
fn walk() -> Vec<Box> {
    let mut boxes: Vec<Box> = Vec::new();
    heap::with_heap(|heap| {
        heap.for_each_live_box(|any, type_name, size, edges| {
            boxes.push(Box {
                any,
                type_name,
                size,
                role: role_of(any),
                edge_addrs: edges.iter().map(|edge| edge.addr()).collect(),
            });
        });
    });
    boxes
}

/// The V8 word for a box, decided by its concrete payload type. `GcAny::is`
/// compares the box's vtable, so this is an exact type test, not a guess from the
/// type's name.
fn role_of(any: GcAny) -> Role {
    if any.is::<JsObject>() {
        Role::Object
    } else if any.is::<crux::Function>() {
        Role::Closure
    } else if any.is::<JsString>() {
        Role::String
    } else if any.is::<crux::Symbol>() {
        Role::Symbol
    } else if any.is::<crux::BigInt>() {
        Role::BigInt
    } else {
        Role::Hidden
    }
}

/// The naming pass, holding the keys it interns once so the walk over it
/// allocates nothing.
struct Naming {
    constructor: PropertyKey,
    name: PropertyKey,
}

impl Naming {
    fn new() -> Self {
        Self {
            constructor: PropertyKey::from_utf8("constructor"),
            name: PropertyKey::from_utf8("name"),
        }
    }

    /// The V8 type word and name for a box.
    fn classify(&self, boxed: &Box) -> (u64, String) {
        match boxed.role {
            Role::Object => (node_type::OBJECT, self.object_name(boxed)),
            Role::Closure => (node_type::CLOSURE, closure_name(boxed)),
            Role::String => (node_type::STRING, String::new()),
            Role::Symbol => (node_type::SYMBOL, String::new()),
            Role::BigInt => (node_type::BIGINT, String::new()),
            Role::Hidden => (node_type::HIDDEN, last_segment(boxed.type_name).to_string()),
        }
    }

    /// The name of an `object` node: its constructor, or its kind when the chain
    /// names none.
    fn object_name(&self, boxed: &Box) -> String {
        // SAFETY: `boxed.any` is a live box (the walk yields live boxes only)
        // and nothing has collected since — the walk and this pass allocate in
        // Rust, not the arena, and the one call here is a data-property read.
        let Some(object) = (unsafe { boxed.any.cast::<JsObject>() }) else {
            return String::new();
        };
        self.constructor_name(object)
            .unwrap_or_else(|| kind_label(&object.kind).to_string())
    }

    /// The nearest own `constructor` on the prototype chain whose `name` is
    /// neither empty nor "Object" — the walk `v8::Object::GetConstructorName`
    /// takes, kept to data properties and away from proxies so the snapshot runs
    /// no host code.
    fn constructor_name(&self, object: Handle<JsObject>) -> Option<String> {
        let mut current = object;
        for _ in 0..MAX_PROTOTYPE_DEPTH {
            if matches!(current.kind, ObjectKind::Proxy(_)) {
                return None;
            }
            if let Ok(Some(property)) = current.get_own_property_key(&self.constructor)
                && let PropertyKind::Data { value, .. } = property.kind
                && let Some(function) = value.as_function()
            {
                let text = self.function_name(function);
                if !text.is_empty() && text != "Object" {
                    return Some(text);
                }
            }
            // The ordinary prototype link, not `get_prototype_of`: a proxy trap
            // is exactly what this avoids, and a proxy is refused above anyway.
            current = current.prototype.get()?;
        }
        None
    }

    /// A constructor's name: its own `name` data property, which is where a
    /// class's name lives, falling back to the function's record of it.
    fn function_name(&self, function: Handle<crux::Function>) -> String {
        if let Ok(Some(property)) = function.object.get_own_property_key(&self.name)
            && let PropertyKind::Data { value, .. } = property.kind
            && let Some(text) = value.as_string()
        {
            return text.to_string_lossy();
        }
        function
            .name
            .as_ref()
            .map(JsString::to_string_lossy)
            .unwrap_or_default()
    }
}

/// A closure's name, from the function the box holds.
fn closure_name(boxed: &Box) -> String {
    // SAFETY: as `Naming::object_name`.
    let Some(function) = (unsafe { boxed.any.cast::<crux::Function>() }) else {
        return String::new();
    };
    function
        .name
        .as_ref()
        .map(JsString::to_string_lossy)
        .unwrap_or_default()
}

/// The engine's own word for an object whose chain named no constructor.
fn kind_label(kind: &ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Ordinary => "Object",
        ObjectKind::Array(_) => "Array",
        ObjectKind::String(_) => "String",
        ObjectKind::Arguments(_) => "Arguments",
        ObjectKind::Proxy(_) => "Proxy",
        ObjectKind::IntegerIndexed(_) => "TypedArray",
        ObjectKind::ModuleNamespace(_) => "Module",
        ObjectKind::IsHTMLDDA => "Object",
        ObjectKind::External(_) => "External",
        ObjectKind::Host(_) => "Object",
    }
}

/// The trailing path segment of a Rust type name (`crux::map::Map` -> `Map`),
/// which is the readable part of it.
fn last_segment(type_name: &str) -> &str {
    type_name.rsplit("::").next().unwrap_or(type_name)
}

/// The document's string table: index 0 is the empty string, as V8's is, and
/// every name is interned to an index.
struct Strings {
    entries: Vec<String>,
    index: HashMap<String, u32>,
}

impl Strings {
    fn new() -> Self {
        let mut index = HashMap::new();
        index.insert(String::new(), 0);
        Self {
            entries: vec![String::new()],
            index,
        }
    }

    fn entries(&self) -> &[String] {
        &self.entries
    }

    fn index_of(&mut self, text: &str) -> u32 {
        if let Some(&existing) = self.index.get(text) {
            return existing;
        }
        let next = self.entries.len() as u32;
        self.entries.push(text.to_string());
        self.index.insert(text.to_string(), next);
        next
    }
}

/// A chunked writer over the host's callback: bytes accumulate until a flush
/// threshold, and a callback that answers `false` stops everything after it.
struct Emitter<F: FnMut(&[u8]) -> bool> {
    callback: F,
    pending: Vec<u8>,
    aborted: bool,
}

impl<F: FnMut(&[u8]) -> bool> Emitter<F> {
    /// How much is buffered before the host is handed a chunk. Large enough that
    /// the callback is not called per number, small enough that the document is
    /// not held in one buffer.
    const FLUSH_AT: usize = 1 << 16;

    fn new(callback: F) -> Self {
        Self {
            callback,
            pending: Vec::with_capacity(Self::FLUSH_AT + 4096),
            aborted: false,
        }
    }

    fn aborted(&self) -> bool {
        self.aborted
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.aborted {
            return;
        }
        self.pending.extend_from_slice(bytes);
        if self.pending.len() >= Self::FLUSH_AT {
            self.flush();
        }
    }

    fn push_byte(&mut self, byte: u8) {
        self.push(&[byte]);
    }

    fn push_str(&mut self, text: &str) {
        self.push(text.as_bytes());
    }

    /// Append `value`'s decimal digits without allocating.
    fn push_u64(&mut self, value: u64) {
        let mut digits = [0u8; 20];
        let mut index = digits.len();
        let mut remaining = value;
        loop {
            index -= 1;
            digits[index] = b'0' + (remaining % 10) as u8;
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        self.push(&digits[index..]);
    }

    fn push_json_string(&mut self, text: &str) {
        self.push_byte(b'"');
        for character in text.chars() {
            match character {
                '"' => self.push_str("\\\""),
                '\\' => self.push_str("\\\\"),
                '\n' => self.push_str("\\n"),
                '\r' => self.push_str("\\r"),
                '\t' => self.push_str("\\t"),
                control if (control as u32) < 0x20 => {
                    let code = control as u32;
                    self.push_str("\\u00");
                    self.push_hex((code >> 4) as u8);
                    self.push_hex((code & 0xF) as u8);
                }
                other => {
                    let mut buffer = [0u8; 4];
                    self.push(other.encode_utf8(&mut buffer).as_bytes());
                }
            }
        }
        self.push_byte(b'"');
    }

    fn push_hex(&mut self, nibble: u8) {
        let digit = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + (nibble - 10)
        };
        self.push_byte(digit);
    }

    /// Hand the buffered bytes to the host, and remember a refusal.
    fn flush(&mut self) {
        if self.aborted || self.pending.is_empty() {
            return;
        }
        let continuing = (self.callback)(&self.pending);
        self.pending.clear();
        if !continuing {
            self.aborted = true;
        }
    }

    fn finish(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{eval, in_context};

    /// The document must have the shape `op_v8_query_objects_count` reads: a
    /// `snapshot.meta` with `node_fields` naming `type` and `name`, a
    /// `node_types` row carrying the `object` word, and flat `nodes`/`strings`
    /// arrays — and a real object must appear in them, named by its constructor
    /// with a size of its own.
    #[test]
    fn the_document_has_the_shape_query_objects_reads() {
        in_context!(scope, {
            eval(
                scope,
                "class SnapshotMarker {} globalThis.snapshotMarker = new SnapshotMarker();",
            );
            let mut bytes = Vec::new();
            scope.take_heap_snapshot(|chunk| {
                bytes.extend_from_slice(chunk);
                true
            });
            let mut scan = Scan::new(&bytes);

            scan.take(b"{\"snapshot\":");
            scan.take(b"{\"meta\":");
            scan.take(b"{");
            scan.field("node_fields");
            let node_fields = scan.string_array();
            // `node_types` is an array whose first element is the type row; the
            // rest of the meta is skipped once that is read.
            scan.field("node_types");
            scan.take(b"[");
            let type_words = scan.string_array();
            scan.field("node_count");
            let node_count = scan.u64();
            scan.field("edge_count");
            let edge_count = scan.u64();

            scan.field("nodes");
            let nodes = scan.u64_array();
            scan.field("edges");
            let edges = scan.u64_array();
            scan.field("strings");
            let strings = scan.string_array();

            let stride = node_fields.len();
            assert!(stride > 0);
            let type_index = node_fields.iter().position(|f| f == "type").expect("type");
            let name_index = node_fields.iter().position(|f| f == "name").expect("name");
            assert_eq!(stride as u64, super::NODE_FIELD_COUNT);
            assert_eq!(node_count, nodes.len() as u64 / stride as u64);
            assert_eq!(edge_count, edges.len() as u64 / 3);
            assert_eq!(nodes.len() % stride, 0, "nodes is strided");
            assert_eq!(edges.len() % 3, 0, "edges is strided");
            assert_eq!(strings.first().map(String::as_str), Some(""));

            let object_type = type_words
                .iter()
                .position(|w| w == "object")
                .expect("the type row must carry the object word queryObjects matches")
                as u64;

            // Every edge points at the start of a node record.
            for edge in edges.chunks_exact(3) {
                let to_node = edge[2];
                assert!(to_node % stride as u64 == 0, "edge target {to_node}");
                assert!(to_node < nodes.len() as u64, "edge target {to_node}");
            }

            // The object a script made is a node of the object type, named by
            // its constructor, with a shallow size of its own.
            let found = nodes.chunks_exact(stride).any(|node| {
                node[type_index] == object_type
                    && strings.get(node[name_index] as usize).map(String::as_str)
                        == Some("SnapshotMarker")
                    && node[3] > 0
            });
            assert!(found, "the newed object is not a named object node");
        });
    }

    /// A callback that refuses a chunk (a host's write error) stops the stream: it
    /// is not called again. The control — a callback that accepts — shows the
    /// snapshot really does stream in more than one chunk, so "stopped" is not an
    /// artefact of a small document.
    #[test]
    fn a_write_error_stops_the_stream() {
        in_context!(scope, {
            eval(
                scope,
                "for (let i = 0; i < 20000; i++) globalThis['o' + i] = { index: i };",
            );
            let mut refused = 0usize;
            scope.take_heap_snapshot(|_| {
                refused += 1;
                false
            });
            assert_eq!(refused, 1, "a refused chunk must stop the stream");

            let mut accepted = 0usize;
            let mut total = 0usize;
            scope.take_heap_snapshot(|chunk| {
                accepted += 1;
                total += chunk.len();
                true
            });
            assert!(accepted > 1, "a large snapshot streams in many chunks");
            assert!(total > 0);
        });
    }

    /// A deliberately small reader for the *exact* document this module emits —
    /// not a general JSON parser. The tests need the `meta` and the three arrays
    /// without adding a JSON crate to this crate's dev-dependencies, and reading
    /// the document independently of the engine's own parser is the point: a bug
    /// in one must not satisfy a test of the other.
    struct Scan<'a> {
        bytes: &'a [u8],
        at: usize,
    }

    impl<'a> Scan<'a> {
        fn new(bytes: &'a [u8]) -> Self {
            Self { bytes, at: 0 }
        }

        fn skip_whitespace(&mut self) {
            while let Some(&byte) = self.bytes.get(self.at) {
                if byte.is_ascii_whitespace() {
                    self.at += 1;
                } else {
                    break;
                }
            }
        }

        fn take(&mut self, literal: &[u8]) {
            self.skip_whitespace();
            assert!(
                self.bytes[self.at..].starts_with(literal),
                "expected {:?} at byte {}",
                std::str::from_utf8(literal),
                self.at
            );
            self.at += literal.len();
        }

        /// Advance to just past the next `"key":` at or after the cursor.
        fn field(&mut self, key: &str) {
            let needle = format!("\"{key}\":");
            let found = self.bytes[self.at..]
                .windows(needle.len())
                .position(|window| window == needle.as_bytes())
                .unwrap_or_else(|| panic!("no field {key:?}"));
            self.at += found + needle.len();
        }

        fn u64(&mut self) -> u64 {
            self.skip_whitespace();
            let start = self.at;
            while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
                self.at += 1;
            }
            std::str::from_utf8(&self.bytes[start..self.at])
                .expect("digits")
                .parse()
                .expect("number")
        }

        fn u64_array(&mut self) -> Vec<u64> {
            self.take(b"[");
            let mut values = Vec::new();
            loop {
                self.skip_whitespace();
                if self.bytes[self.at] == b']' {
                    self.at += 1;
                    break;
                }
                values.push(self.u64());
                self.skip_whitespace();
                if self.bytes[self.at] == b',' {
                    self.at += 1;
                }
            }
            values
        }

        fn string(&mut self) -> String {
            self.skip_whitespace();
            self.take(b"\"");
            let mut text = String::new();
            loop {
                let byte = self.bytes[self.at];
                self.at += 1;
                match byte {
                    b'"' => break,
                    b'\\' => {
                        let escape = self.bytes[self.at];
                        self.at += 1;
                        match escape {
                            b'"' => text.push('"'),
                            b'\\' => text.push('\\'),
                            b'n' => text.push('\n'),
                            b'r' => text.push('\r'),
                            b't' => text.push('\t'),
                            b'u' => {
                                let hex = std::str::from_utf8(&self.bytes[self.at..self.at + 4])
                                    .expect("hex");
                                self.at += 4;
                                let code = u16::from_str_radix(hex, 16).expect("code point");
                                text.push(char::from_u32(u32::from(code)).unwrap_or('\u{FFFD}'));
                            }
                            other => panic!("unexpected escape {other}"),
                        }
                    }
                    _ => {
                        let start = self.at - 1;
                        let width = utf8_width(byte);
                        self.at += width - 1;
                        text.push_str(
                            std::str::from_utf8(&self.bytes[start..self.at]).expect("utf-8"),
                        );
                    }
                }
            }
            text
        }

        fn string_array(&mut self) -> Vec<String> {
            self.take(b"[");
            let mut values = Vec::new();
            loop {
                self.skip_whitespace();
                if self.bytes[self.at] == b']' {
                    self.at += 1;
                    break;
                }
                values.push(self.string());
                self.skip_whitespace();
                if self.bytes[self.at] == b',' {
                    self.at += 1;
                }
            }
            values
        }
    }

    /// The length in bytes of the UTF-8 sequence whose lead byte is `first`.
    fn utf8_width(first: u8) -> usize {
        match first {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            _ => 4,
        }
    }
}
