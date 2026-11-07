//! The C-side `JSValue` representation and reference-counted boxes.
//!
//! QuickJS's `JSValue` is a 16-byte struct `{ union; int64_t tag }` on 64-bit
//! (both QuickJS-NG and Bellard, since `JS_PTR64` is auto-defined). We model
//! the union as its widest member, `u64`, and type-pun through `to_bits` /
//! `from_bits` for floats and pointer casts for box pointers. That is
//! ABI-identical to the C union and keeps every union read well-defined.
//!
//! Reference-counted tags carry a pointer to a [`RefBox`]. A box holds the
//! engine `Value` it wraps and a reference count; the box registry is a GC
//! root source, so a value the host holds through a `JSValue` stays alive for
//! as long as the box does — the same promise `crates/ffi`'s tables make.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use crux::heap::{GcAny, RootSource, Trace};
use crux::value::{Value as CruxValue, ValueKind};

/// The C `JS_BOOL` (quickjs.h): `bool` in QuickJS-NG, `int` in Bellard. The
/// return ABI differs (1-byte `al` vs 4-byte `eax`), so predicates that answer
/// `JS_BOOL` must return this type rather than a Rust `bool`.
#[cfg(feature = "ng")]
pub type JS_BOOL = bool;
#[cfg(feature = "bellard")]
pub type JS_BOOL = i32;

/// Convert a Rust `bool` to the C `JS_BOOL` type.
pub fn js_bool(value: bool) -> JS_BOOL {
    #[cfg(feature = "ng")]
    {
        value
    }
    #[cfg(feature = "bellard")]
    {
        i32::from(value)
    }
}

// The tag values both targets share on 64-bit (quickjs.h).
pub const JS_TAG_BIG_INT: i64 = -9;
pub const JS_TAG_SYMBOL: i64 = -8;
pub const JS_TAG_STRING: i64 = -7;
pub const JS_TAG_STRING_ROPE: i64 = -6;
pub const JS_TAG_MODULE: i64 = -3;
pub const JS_TAG_FUNCTION_BYTECODE: i64 = -2;
pub const JS_TAG_OBJECT: i64 = -1;
pub const JS_TAG_INT: i64 = 0;
pub const JS_TAG_BOOL: i64 = 1;
pub const JS_TAG_NULL: i64 = 2;
pub const JS_TAG_UNDEFINED: i64 = 3;
pub const JS_TAG_UNINITIALIZED: i64 = 4;
pub const JS_TAG_CATCH_OFFSET: i64 = 5;
pub const JS_TAG_EXCEPTION: i64 = 6;
pub const JS_TAG_SHORT_BIG_INT: i64 = 7;
pub const JS_TAG_FLOAT64: i64 = 8;

/// The C `JSValue`: `{ JSValueUnion u; int64_t tag; }`. The union is carried
/// as its widest member; a float is stored bit-for-bit and a pointer as its
/// address, matching the C layout exactly.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JSValue {
    pub u: u64,
    pub tag: i64,
}

/// The value a guarded FFI call returns after a panic: the exception sentinel,
/// so a C host sees an exception rather than a forged zero-int.
impl Default for JSValue {
    fn default() -> Self {
        exception()
    }
}

/// Which kind of box a reference-counted `JSValue` names.
#[derive(Clone, Copy)]
pub enum RefKind {
    /// An object (or function), keyed by its stable object id for
    /// canonicalization so two refs to one object share a box pointer.
    Object(u64),
    String,
    Symbol,
    BigInt,
    Module,
    Bytecode,
}

/// A reference-counted box holding an engine value the host still has a
/// `JSValue` for.
pub struct RefBox {
    refs: Cell<u32>,
    kind: RefKind,
    value: CruxValue,
}

/// The live boxes, keyed by their (stable) heap address. Object boxes are also
/// keyed by object id so a second `JSValue` for the same object reuses the box.
#[derive(Default)]
struct BoxTable {
    by_ptr: HashMap<usize, Box<RefBox>>,
    object_box: HashMap<u64, usize>,
}

thread_local! {
    static BOXES: RefCell<BoxTable> = RefCell::new(BoxTable::default());
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
}

/// The registry as a root source: each box's engine value is visited, so a
/// host-held value survives collections the host's own memory cannot be seen
/// by.
struct BoxRoots;

impl RootSource for BoxRoots {
    fn roots(&self, visit: &mut dyn FnMut(GcAny)) {
        BOXES.with(|boxes| {
            for entry in boxes.borrow().by_ptr.values() {
                entry.value.trace(visit);
            }
        });
    }
}

static BOX_ROOTS: BoxRoots = BoxRoots;

fn register() {
    REGISTERED.with(|registered| {
        if registered.replace(true) {
            return;
        }
        crux::heap::register_root_source(&BOX_ROOTS);
    });
}

fn tag_of(kind: RefKind) -> i64 {
    match kind {
        RefKind::Object(_) => JS_TAG_OBJECT,
        RefKind::String => JS_TAG_STRING,
        RefKind::Symbol => JS_TAG_SYMBOL,
        RefKind::BigInt => JS_TAG_BIG_INT,
        RefKind::Module => JS_TAG_MODULE,
        RefKind::Bytecode => JS_TAG_FUNCTION_BYTECODE,
    }
}

/// Store a value in a fresh box, returning the box pointer.
fn insert_box(kind: RefKind, value: CruxValue) -> *mut std::ffi::c_void {
    register();
    let boxed = Box::new(RefBox {
        refs: Cell::new(1),
        kind,
        value,
    });
    let ptr = &*boxed as *const RefBox as *mut std::ffi::c_void;
    BOXES.with(|boxes| {
        let mut boxes = boxes.borrow_mut();
        if let RefKind::Object(id) = kind {
            boxes.object_box.insert(id, ptr as usize);
        }
        boxes.by_ptr.insert(ptr as usize, boxed);
    });
    ptr
}

/// Add a reference to a live box.
pub fn dup_box(ptr: *mut std::ffi::c_void) {
    BOXES.with(|boxes| {
        if let Some(entry) = boxes.borrow().by_ptr.get(&(ptr as usize)) {
            entry.refs.set(entry.refs.get() + 1);
        }
    });
}

/// Drop a reference; the last one removes and drops the box.
pub fn free_box(ptr: *mut std::ffi::c_void) {
    BOXES.with(|boxes| {
        let mut boxes = boxes.borrow_mut();
        let Some(entry) = boxes.by_ptr.get(&(ptr as usize)) else {
            return;
        };
        let refs = entry.refs.get();
        if refs > 1 {
            entry.refs.set(refs - 1);
            return;
        }
        let kind = entry.kind;
        boxes.by_ptr.remove(&(ptr as usize));
        if let RefKind::Object(id) = kind {
            boxes.object_box.remove(&id);
        }
    });
}

/// The engine value a live box holds.
fn box_value(ptr: usize) -> Option<CruxValue> {
    BOXES.with(|boxes| boxes.borrow().by_ptr.get(&ptr).map(|entry| entry.value))
}

fn is_refcounted(tag: i64) -> bool {
    matches!(
        tag,
        JS_TAG_OBJECT
            | JS_TAG_STRING
            | JS_TAG_STRING_ROPE
            | JS_TAG_SYMBOL
            | JS_TAG_BIG_INT
            | JS_TAG_MODULE
            | JS_TAG_FUNCTION_BYTECODE
    )
}

/// `JS_DupValue`: add a reference to a refcounted value; primitives are no-ops.
pub fn dup_jsvalue(value: JSValue) -> JSValue {
    if value.tag == JS_TAG_MODULE {
        crate::c::module::dup_module_value(value.u as usize);
    } else if is_refcounted(value.tag) {
        dup_box(value.u as usize as *mut std::ffi::c_void);
    }
    value
}

/// `JS_FreeValue`: drop a reference; primitives are no-ops.
pub fn free_jsvalue(value: JSValue) {
    if value.tag == JS_TAG_MODULE {
        crate::c::module::free_module_value(value.u as usize);
    } else if is_refcounted(value.tag) {
        free_box(value.u as usize as *mut std::ffi::c_void);
    }
}

/// Build a boxed `JSValue` for a non-canonicalized kind.
fn boxed(kind: RefKind, value: CruxValue) -> JSValue {
    let ptr = insert_box(kind, value);
    JSValue {
        u: ptr as usize as u64,
        tag: tag_of(kind),
    }
}

/// Build (or reuse) the box for an object value, canonicalized by object id.
fn object_box(id: u64, value: CruxValue) -> JSValue {
    register();
    if let Some(ptr) = BOXES.with(|boxes| boxes.borrow().object_box.get(&id).copied()) {
        dup_box(ptr as *mut std::ffi::c_void);
        return JSValue {
            u: ptr as u64,
            tag: JS_TAG_OBJECT,
        };
    }
    let ptr = insert_box(RefKind::Object(id), value);
    JSValue {
        u: ptr as usize as u64,
        tag: JS_TAG_OBJECT,
    }
}

/// Convert an engine value to its C `JSValue`. Reference-counted values get a
/// box; primitives are the header's exact bits.
pub fn value_to_jsvalue(value: CruxValue) -> JSValue {
    match value.kind() {
        ValueKind::Number(number) => JSValue {
            u: number.to_bits(),
            tag: JS_TAG_FLOAT64,
        },
        ValueKind::Boolean(boolean) => JSValue {
            u: u64::from(boolean),
            tag: JS_TAG_BOOL,
        },
        ValueKind::Undefined => JSValue {
            u: 0,
            tag: JS_TAG_UNDEFINED,
        },
        ValueKind::Null => JSValue {
            u: 0,
            tag: JS_TAG_NULL,
        },
        ValueKind::String(_) => boxed(RefKind::String, value),
        ValueKind::Symbol(_) => boxed(RefKind::Symbol, value),
        ValueKind::BigInt(_) => boxed(RefKind::BigInt, value),
        ValueKind::Object(handle) => object_box(handle.id(), value),
        ValueKind::Function(handle) => object_box(handle.object.id(), value),
    }
}

/// Convert a C `JSValue` back to its engine value. `None` for a null/forged
/// box pointer, an unknown tag, or the C-side exception sentinel.
pub fn jsvalue_to_value(value: JSValue) -> Option<CruxValue> {
    match value.tag {
        JS_TAG_INT => Some(CruxValue::Number(value.u as i32 as f64)),
        JS_TAG_BOOL => Some(CruxValue::Boolean(value.u != 0)),
        JS_TAG_NULL => Some(CruxValue::Null),
        JS_TAG_UNDEFINED => Some(CruxValue::Undefined),
        JS_TAG_FLOAT64 => Some(CruxValue::Number(f64::from_bits(value.u))),
        JS_TAG_OBJECT
        | JS_TAG_STRING
        | JS_TAG_STRING_ROPE
        | JS_TAG_SYMBOL
        | JS_TAG_BIG_INT
        | JS_TAG_MODULE
        | JS_TAG_FUNCTION_BYTECODE => box_value(value.u as usize),
        _ => None,
    }
}

/// The C `JSValue` for an int32 (the header's inline `JS_NewInt32`).
pub fn int32(value: i32) -> JSValue {
    JSValue {
        u: value as u32 as u64,
        tag: JS_TAG_INT,
    }
}

/// The C `JSValue` for a bool (the header's inline `JS_NewBool`).
pub fn boolean(value: bool) -> JSValue {
    JSValue {
        u: u64::from(value),
        tag: JS_TAG_BOOL,
    }
}

/// The C `JSValue` for a float64 (the header's inline `JS_NewFloat64`).
pub fn float64(value: f64) -> JSValue {
    JSValue {
        u: value.to_bits(),
        tag: JS_TAG_FLOAT64,
    }
}

pub const fn undefined() -> JSValue {
    JSValue {
        u: 0,
        tag: JS_TAG_UNDEFINED,
    }
}

pub const fn null() -> JSValue {
    JSValue {
        u: 0,
        tag: JS_TAG_NULL,
    }
}

pub const fn exception() -> JSValue {
    JSValue {
        u: 0,
        tag: JS_TAG_EXCEPTION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crux::handle::Handle;
    use crux::string::JsString;

    #[test]
    fn jsvalue_has_the_c_layout() {
        assert_eq!(std::mem::size_of::<JSValue>(), 16);
        assert_eq!(std::mem::align_of::<JSValue>(), 8);
        assert_eq!(std::mem::offset_of!(JSValue, tag), 8);
    }

    #[test]
    fn primitives_round_trip() {
        let number = value_to_jsvalue(CruxValue::Number(3.5));
        assert_eq!(number.tag, JS_TAG_FLOAT64);
        assert_eq!(jsvalue_to_value(number), Some(CruxValue::Number(3.5)));

        let truth = value_to_jsvalue(CruxValue::Boolean(true));
        assert_eq!(truth.tag, JS_TAG_BOOL);
        assert_eq!(jsvalue_to_value(truth), Some(CruxValue::Boolean(true)));

        assert_eq!(
            jsvalue_to_value(value_to_jsvalue(CruxValue::Null)),
            Some(CruxValue::Null)
        );
        assert_eq!(
            jsvalue_to_value(value_to_jsvalue(CruxValue::Undefined)),
            Some(CruxValue::Undefined)
        );
    }

    #[test]
    fn int32_encodes_the_headers_bits() {
        let value = int32(42);
        assert_eq!(value.tag, JS_TAG_INT);
        assert_eq!(value.u, 42);
        assert_eq!(jsvalue_to_value(value), Some(CruxValue::Number(42.0)));
    }

    #[test]
    fn the_exception_sentinel_is_not_a_value() {
        assert_eq!(jsvalue_to_value(exception()), None);
    }

    #[test]
    fn string_boxes_refcount_and_free() {
        let handle = Handle::new(JsString::from_utf8("boxed"));
        let value = value_to_jsvalue(CruxValue::String(handle));
        assert_eq!(value.tag, JS_TAG_STRING);
        let ptr = value.u as usize as *mut std::ffi::c_void;

        dup_box(ptr);
        free_box(ptr);
        assert_eq!(
            jsvalue_to_value(value).and_then(|v| v.as_string().map(|s| s.to_string_lossy())),
            Some("boxed".to_string())
        );

        free_box(ptr);
        assert!(jsvalue_to_value(value).is_none());
    }

    #[test]
    fn a_retained_box_roots_its_value_through_a_collection() {
        let handle = Handle::new(JsString::from_utf8("retained"));
        let box_addr = handle.as_any().addr();
        let value = value_to_jsvalue(CruxValue::String(handle));

        let swept = crux::heap::with_heap_mut(|heap| heap.collect(&[]));
        assert!(
            !swept.contains(&box_addr),
            "the box a host still holds was swept: {swept:?}"
        );
        assert_eq!(
            jsvalue_to_value(value).and_then(|v| v.as_string().map(|s| s.to_string_lossy())),
            Some("retained".to_string())
        );
    }
}
