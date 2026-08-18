//! ECMAScript language values (spec 6.1), NaN-boxed into a single machine
//! word (PLAN Phase 18 performance milestone).

use std::fmt;
use std::rc::Rc;

use crate::bigint::BigInt;
use crate::function::Function;
use crate::handle::Handle;
use crate::object::JsObject;
use crate::string::JsString;
use crate::symbol::Symbol;

/// A value is *tagged* when its top 16 bits are `0x7FF8` — a quiet NaN whose
/// payload bits 50-48 are zero. The tag occupies bits 47-44 and the payload
/// bits 43-0 (an `Rc` pointer for heap values). Every other bit pattern is a
/// double, stored exactly: signaling NaNs and quiet NaNs with bits 50-48
/// non-zero survive as-is. A quiet NaN whose top 16 bits are exactly `0x7FF8`
/// collides with the tag region and is canonicalized to `0x7FF9_0000_0000_0000`
/// on box; JS cannot observe a NaN payload, so this is unobservable.
const TAG_MASK: u64 = 0xFFFF_0000_0000_0000;
const TAG_PREFIX: u64 = 0x7FF8_0000_0000_0000;
const CANON_NAN: u64 = 0x7FF9_0000_0000_0000;
const PAYLOAD_MASK: u64 = (1 << 44) - 1;

const TAG_UNDEFINED: u64 = 0;
const TAG_NULL: u64 = 1;
const TAG_FALSE: u64 = 2;
const TAG_TRUE: u64 = 3;
const TAG_BIGINT: u64 = 4;
const TAG_STRING: u64 = 5;
const TAG_SYMBOL: u64 = 6;
const TAG_OBJECT: u64 = 7;
const TAG_FUNCTION: u64 = 8;

/// An ECMAScript language value (spec 6.1).
///
/// NaN-boxed: a double when the top 16 bits are not `0x7FF8`, otherwise a tag
/// plus payload (see the layout note above). Heap values own one strong `Rc`
/// ref; `Clone`/`Drop` reconstruct the `Rc` from the payload so refcounts stay
/// exact. `PartialEq` preserves the derived-enum semantics: `Number` compares
/// via `f64::eq` (`NaN != NaN`), objects via their id equality.
pub struct Value(u64, std::marker::PhantomData<Rc<()>>);

// The box holds raw `Rc` pointers, so it must not cross threads (the refcount
// is not atomic) — matching the `Rc`-based enum it replaces.

/// The logical variant of a value, mirroring the pre-NaN-boxing enum so
/// `match value.kind()` keeps the old arm shapes (with `ValueKind::` in place
/// of `Value::` in patterns). The heap variants hold clones of the handles.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueKind {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    BigInt(Handle<BigInt>),
    String(Handle<JsString>),
    Symbol(Handle<Symbol>),
    Object(Handle<JsObject>),
    Function(Handle<Function>),
}

#[allow(non_snake_case, non_upper_case_globals)]
impl Value {
    // Constructors keep the pre-NaN-boxing variant spellings so the ~4,500
    // construction sites compile unchanged (clippy wants snake_case).
    pub const Undefined: Value =
        Value(TAG_PREFIX | (TAG_UNDEFINED << 44), std::marker::PhantomData);
    pub const Null: Value = Value(TAG_PREFIX | (TAG_NULL << 44), std::marker::PhantomData);

    pub fn Boolean(b: bool) -> Value {
        let tag = if b { TAG_TRUE } else { TAG_FALSE };
        Value(TAG_PREFIX | (tag << 44), std::marker::PhantomData)
    }

    pub fn Number(n: f64) -> Value {
        let bits = n.to_bits();
        // Colliding quiet NaNs (top 16 bits 0x7FF8) would read as a tag.
        let bits = if bits & TAG_MASK == TAG_PREFIX {
            CANON_NAN
        } else {
            bits
        };
        Value(bits, std::marker::PhantomData)
    }

    pub fn BigInt(h: Handle<BigInt>) -> Value {
        Value::box_heap(TAG_BIGINT, h)
    }

    pub fn String(h: Handle<JsString>) -> Value {
        Value::box_heap(TAG_STRING, h)
    }

    pub fn Symbol(h: Handle<Symbol>) -> Value {
        Value::box_heap(TAG_SYMBOL, h)
    }

    pub fn Object(h: Handle<JsObject>) -> Value {
        Value::box_heap(TAG_OBJECT, h)
    }

    pub fn Function(h: Handle<Function>) -> Value {
        Value::box_heap(TAG_FUNCTION, h)
    }

    /// Leak one strong ref into the box and store its pointer in the payload.
    fn box_heap<T>(tag: u64, h: Handle<T>) -> Value {
        let ptr = Rc::into_raw(h) as usize as u64;
        debug_assert!(
            ptr & !PAYLOAD_MASK == 0,
            "pointer exceeds the 44-bit payload"
        );
        Value(
            TAG_PREFIX | (tag << 44) | (ptr & PAYLOAD_MASK),
            std::marker::PhantomData,
        )
    }

    fn tag(&self) -> u64 {
        (self.0 >> 44) & 0xF
    }

    fn payload(&self) -> u64 {
        self.0 & PAYLOAD_MASK
    }

    /// Whether the bits hold a double (anything outside the tag region).
    fn is_double(&self) -> bool {
        self.0 & TAG_MASK != TAG_PREFIX
    }

    /// Reconstruct the box's leaked ref, clone it for the caller, and leak
    /// the box's ref back.
    unsafe fn take_ref<T>(&self, tag: u64) -> Option<Handle<T>> {
        if self.tag() == tag {
            // SAFETY: the payload holds a ref leaked by `box_heap`'s
            // `Rc::into_raw`; `forget` below leaks it back.
            let rc = unsafe { Rc::from_raw(self.payload() as usize as *const T) };
            let out = Rc::clone(&rc);
            std::mem::forget(rc);
            Some(out)
        } else {
            None
        }
    }

    /// The logical variant (cloned handles for heap values).
    pub fn kind(&self) -> ValueKind {
        if self.is_double() {
            ValueKind::Number(f64::from_bits(self.0))
        } else {
            match self.tag() {
                TAG_UNDEFINED => ValueKind::Undefined,
                TAG_NULL => ValueKind::Null,
                TAG_FALSE => ValueKind::Boolean(false),
                TAG_TRUE => ValueKind::Boolean(true),
                TAG_BIGINT => ValueKind::BigInt(unsafe { self.take_ref(TAG_BIGINT) }.unwrap()),
                TAG_STRING => ValueKind::String(unsafe { self.take_ref(TAG_STRING) }.unwrap()),
                TAG_SYMBOL => ValueKind::Symbol(unsafe { self.take_ref(TAG_SYMBOL) }.unwrap()),
                TAG_OBJECT => ValueKind::Object(unsafe { self.take_ref(TAG_OBJECT) }.unwrap()),
                TAG_FUNCTION => {
                    ValueKind::Function(unsafe { self.take_ref(TAG_FUNCTION) }.unwrap())
                }
                _ => unreachable!("reserved tag"),
            }
        }
    }

    pub fn is_undefined(&self) -> bool {
        self.tag() == TAG_UNDEFINED && !self.is_double()
    }

    pub fn is_null(&self) -> bool {
        self.tag() == TAG_NULL
    }

    pub fn is_boolean(&self) -> bool {
        matches!(self.tag(), TAG_FALSE | TAG_TRUE)
    }

    pub fn is_number(&self) -> bool {
        self.is_double()
    }

    pub fn is_bigint(&self) -> bool {
        self.tag() == TAG_BIGINT
    }

    pub fn is_string(&self) -> bool {
        self.tag() == TAG_STRING
    }

    pub fn is_symbol(&self) -> bool {
        self.tag() == TAG_SYMBOL
    }

    pub fn is_object(&self) -> bool {
        self.tag() == TAG_OBJECT
    }

    pub fn is_function(&self) -> bool {
        self.tag() == TAG_FUNCTION
    }

    pub fn as_number(&self) -> Option<f64> {
        if self.is_double() {
            Some(f64::from_bits(self.0))
        } else {
            None
        }
    }

    pub fn as_boolean(&self) -> Option<bool> {
        match self.tag() {
            TAG_FALSE => Some(false),
            TAG_TRUE => Some(true),
            _ => None,
        }
    }

    pub fn as_bigint(&self) -> Option<Handle<BigInt>> {
        unsafe { self.take_ref(TAG_BIGINT) }
    }

    pub fn as_string(&self) -> Option<Handle<JsString>> {
        unsafe { self.take_ref(TAG_STRING) }
    }

    pub fn as_symbol(&self) -> Option<Handle<Symbol>> {
        unsafe { self.take_ref(TAG_SYMBOL) }
    }

    /// The object handle when `self` is an Object value; `None` otherwise.
    /// Function values wrap their object side separately and report `None`.
    pub fn as_object(&self) -> Option<Handle<JsObject>> {
        unsafe { self.take_ref(TAG_OBJECT) }
    }

    pub fn as_function(&self) -> Option<Handle<Function>> {
        unsafe { self.take_ref(TAG_FUNCTION) }
    }
}

impl Clone for Value {
    fn clone(&self) -> Value {
        match self.kind() {
            ValueKind::Undefined => Value::Undefined,
            ValueKind::Null => Value::Null,
            ValueKind::Boolean(b) => Value::Boolean(b),
            ValueKind::Number(n) => Value::Number(n),
            ValueKind::BigInt(h) => Value::BigInt(h),
            ValueKind::String(h) => Value::String(h),
            ValueKind::Symbol(h) => Value::Symbol(h),
            ValueKind::Object(h) => Value::Object(h),
            ValueKind::Function(h) => Value::Function(h),
        }
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        if self.is_double() {
            return;
        }
        unsafe fn release<T>(payload: u64) {
            // SAFETY: the payload holds a ref leaked by `box_heap`.
            drop(unsafe { Rc::from_raw(payload as usize as *const T) });
        }
        unsafe {
            match self.tag() {
                TAG_BIGINT => release::<BigInt>(self.payload()),
                TAG_STRING => release::<JsString>(self.payload()),
                TAG_SYMBOL => release::<Symbol>(self.payload()),
                TAG_OBJECT => release::<JsObject>(self.payload()),
                TAG_FUNCTION => release::<Function>(self.payload()),
                _ => {}
            }
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        self.kind() == other.kind()
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.kind(), f)
    }
}

impl fmt::Display for Value {
    /// Diagnostics-only rendering; JavaScript string conversion is
    /// `convert::to_string`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            ValueKind::Undefined => f.write_str("undefined"),
            ValueKind::Null => f.write_str("null"),
            ValueKind::Boolean(b) => write!(f, "{b}"),
            ValueKind::Number(n) => write!(f, "{n}"),
            ValueKind::BigInt(b) => write!(f, "{}", b.0),
            ValueKind::String(s) => write!(f, "{s}"),
            ValueKind::Symbol(s) => write!(f, "{}", crate::symbol::descriptive_string(&s)),
            ValueKind::Object(_) => f.write_str("[object Object]"),
            ValueKind::Function(fun) => match &fun.name {
                Some(name) => write!(f, "function {name}"),
                None => f.write_str("function"),
            },
        }
    }
}

/// The `Type` abstract operation (spec 7.2.1). Proxies over callable
/// functions report `function` like the spec's typeof.
pub fn type_of(value: &Value) -> &'static str {
    if value.is_double() {
        return "number";
    }
    match value.tag() {
        TAG_UNDEFINED => "undefined",
        TAG_NULL => "null",
        TAG_FALSE | TAG_TRUE => "boolean",
        TAG_BIGINT => "bigint",
        TAG_STRING => "string",
        TAG_SYMBOL => "symbol",
        TAG_FUNCTION => "function",
        TAG_OBJECT => match value.as_object() {
            Some(obj) => match &obj.kind {
                crate::object::ObjectKind::IsHTMLDDA => "undefined",
                crate::object::ObjectKind::Proxy(slots)
                    if slots
                        .target
                        .borrow()
                        .as_ref()
                        .map(is_callable)
                        .unwrap_or(false) =>
                {
                    "function"
                }
                _ => "object",
            },
            None => "object",
        },
        _ => unreachable!("reserved tag"),
    }
}

/// `IsCallable` (spec 7.2.3): function values and proxies whose target was
/// callable at creation (ProxyCreate, spec 10.5.15 step 10 — revocation
/// does not remove the proxy's [[Call]] internal method).
pub fn is_callable(value: &Value) -> bool {
    if value.is_function() {
        return true;
    }
    if let Some(obj) = value.as_object() {
        return match &obj.kind {
            crate::object::ObjectKind::IsHTMLDDA => true,
            crate::object::ObjectKind::Proxy(slots) => slots.callable.get(),
            _ => false,
        };
    }
    false
}

/// `IsConstructor` (spec 7.2.4): built-ins with a [[Construct]], ECMAScript
/// (non-arrow) functions, bound functions, and proxies whose target was a
/// constructor at creation.
pub fn is_constructor(value: &Value) -> bool {
    if value.is_function() {
        return value
            .as_function()
            .map(|function| function.is_constructor())
            .unwrap_or(false);
    }
    if let Some(obj) = value.as_object() {
        return match &obj.kind {
            crate::object::ObjectKind::Proxy(slots) => slots.constructible.get(),
            _ => false,
        };
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(n: f64) -> Value {
        Value::Number(n)
    }

    #[test]
    fn type_of_all_variants() {
        assert_eq!(type_of(&Value::Undefined), "undefined");
        assert_eq!(type_of(&Value::Null), "null");
        assert_eq!(type_of(&Value::Boolean(true)), "boolean");
        assert_eq!(type_of(&number(1.0)), "number");
        assert_eq!(
            type_of(&Value::BigInt(Handle::new(BigInt::from(1)))),
            "bigint"
        );
        assert_eq!(
            type_of(&Value::String(Handle::new(JsString::from_utf8("x")))),
            "string"
        );
        assert_eq!(
            type_of(&Value::Symbol(Handle::new(Symbol::new(None)))),
            "symbol"
        );
        assert_eq!(
            type_of(&Value::Object(JsObject::ordinary_object_create(None))),
            "object"
        );
        assert_eq!(type_of(&Value::Function(Function::new(None))), "function");
    }

    #[test]
    fn primitives_are_not_callable_or_constructible() {
        for v in [
            Value::Undefined,
            Value::Null,
            Value::Boolean(false),
            number(0.0),
            Value::BigInt(Handle::new(BigInt::from(0))),
            Value::String(Handle::new(JsString::from_utf8(""))),
            Value::Symbol(Handle::new(Symbol::new(None))),
        ] {
            assert!(!is_callable(&v));
            assert!(!is_constructor(&v));
        }
    }

    #[test]
    fn display_for_diagnostics() {
        assert_eq!(Value::Undefined.to_string(), "undefined");
        assert_eq!(Value::Null.to_string(), "null");
        assert_eq!(Value::Boolean(true).to_string(), "true");
        assert_eq!(number(1.5).to_string(), "1.5");
        assert_eq!(
            Value::String(Handle::new(JsString::from_utf8("hi"))).to_string(),
            "hi"
        );
        assert_eq!(
            Value::Symbol(Handle::new(Symbol::new(Some(JsString::from_utf8("k"))))).to_string(),
            "Symbol(k)"
        );
    }

    #[test]
    fn numbers_round_trip_including_nan_and_negative_zero() {
        for n in [
            0.0,
            -0.0,
            1.5,
            -1e300,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ] {
            let v = Value::Number(n);
            assert!(v.is_number());
            let back = v.as_number().unwrap();
            if n.is_nan() {
                assert!(back.is_nan());
            } else {
                assert_eq!(back, n);
            }
        }
        assert_eq!(
            Value::Number(-0.0).as_number().unwrap().to_bits(),
            (-0.0f64).to_bits()
        );
        // Signaling NaNs and quiet NaNs outside the tag region survive as-is.
        for bits in [
            0x7FF0_0000_0000_0001u64,
            0x7FF9_0000_0000_0001u64,
            0xFFFF_FFFF_FFFF_FFFFu64,
        ] {
            let v = Value::Number(f64::from_bits(bits));
            assert!(v.is_number());
            assert_eq!(v.as_number().unwrap().to_bits(), bits);
        }
    }

    #[test]
    fn canonical_nan_is_never_mistagged() {
        // f64::NAN's bit pattern (0x7FF8_0000_0000_0000) is the undefined tag
        // region; boxing must canonicalize it so it unboxes as NaN, not a tag.
        let n = Value::Number(f64::NAN);
        assert!(n.is_number());
        assert!(n.as_number().unwrap().is_nan());
        assert_ne!(n.as_number().unwrap().to_bits(), f64::NAN.to_bits());
    }

    #[test]
    fn tags_round_trip() {
        assert!(Value::Undefined.is_undefined());
        assert!(!Value::Undefined.is_null());
        assert!(Value::Null.is_null());
        assert!(Value::Boolean(true).is_boolean());
        assert_eq!(Value::Boolean(true).as_boolean(), Some(true));
        assert_eq!(Value::Boolean(false).as_boolean(), Some(false));
        assert!(Value::Number(3.0).is_number());
        assert!(!Value::Number(3.0).is_object());

        let b = Value::BigInt(Handle::new(BigInt::from(7)));
        assert!(b.is_bigint());
        assert_eq!(b.as_bigint().unwrap().0, num_bigint::BigInt::from(7));

        let s = Value::String(Handle::new(JsString::from_utf8("x")));
        assert!(s.is_string());
        assert_eq!(*s.as_string().unwrap(), JsString::from_utf8("x"));

        let sym = Value::Symbol(Handle::new(Symbol::new(None)));
        assert!(sym.is_symbol());
        assert!(sym.as_symbol().is_some());

        let o = Value::Object(JsObject::ordinary_object_create(None));
        assert!(o.is_object());
        assert!(o.as_object().is_some());
        assert!(o.as_function().is_none());

        let f = Value::Function(Function::new(None));
        assert!(f.is_function());
        assert!(f.as_function().is_some());
        assert!(is_callable(&f));
        assert!(matches!(f.kind(), ValueKind::Function(_)));
    }

    #[test]
    fn clone_and_drop_maintain_refcounts() {
        let obj = JsObject::ordinary_object_create(None);
        let v = Value::Object(obj.clone());
        assert_eq!(Handle::strong_count(&obj), 2); // obj handle + the box's ref
        let c = v.clone();
        assert_eq!(Handle::strong_count(&obj), 3);
        drop(c);
        assert_eq!(Handle::strong_count(&obj), 2);
        drop(v);
        assert_eq!(Handle::strong_count(&obj), 1);
    }

    #[test]
    fn partial_eq_matches_enum_semantics() {
        assert_eq!(Value::Undefined, Value::Undefined);
        assert_ne!(Value::Undefined, Value::Null);
        assert_eq!(Value::Boolean(true), Value::Boolean(true));
        assert_ne!(Value::Boolean(true), Value::Boolean(false));
        assert_eq!(Value::Number(-0.0), Value::Number(0.0));
        assert_ne!(Value::Number(f64::NAN), Value::Number(f64::NAN));
        let a = JsObject::ordinary_object_create(None);
        assert_eq!(Value::Object(a.clone()), Value::Object(a.clone()));
        let b = JsObject::ordinary_object_create(None);
        assert_ne!(Value::Object(a.clone()), Value::Object(b));
    }
}
