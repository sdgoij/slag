//! JavaScript strings: UTF-16 code-unit sequences (spec 6.1.4) and the string
//! interner backing `AtomId`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use crate::handle::Handle;
use crate::heap::{GcAny, Trace};

/// A JavaScript string: a sequence of UTF-16 code units (spec 6.1.4).
///
/// A string is either a single contiguous buffer or a rope — a binary tree of
/// concatenation nodes. `concat` appends in O(1) once the accumulated string
/// is large enough that repeated copying would dominate, and the contiguous
/// form is materialized lazily on first access (and cached, since strings are
/// immutable). `len` is O(1) for both forms.
///
/// The rope node IS the box the value points at (`Handle<JsString>`), so an
/// append is a single allocation; the children are the operands' own handles
/// (Rc bumps, no copies). That makes ropes `!Send`, which is fine: the one
/// `Send` consumer — the well-known-symbol table — keeps its symbols
/// thread-locally instead. Flat buffers are `Arc`-shared, so flat clones are
/// O(1). The rope tracks a tree depth and folds an over-deep left side into a
/// single shared flat; both the fold and the final drop are amortized across
/// the cap appends, so arbitrarily long append chains stay linear.
pub enum JsString {
    Flat(Arc<[u16]>),
    /// Cut 67: a string of at most [`SMALL_STRING_CAP`] code units stored
    /// INLINE in the box — one arena allocation instead of the Vec + Arc +
    /// box the `Flat` path pays for tiny strings (the concat and literal
    /// paths allocate them constantly). `as_slice` borrows the box, which is
    /// stable (the arena never moves boxes), so the slice is valid while the
    /// owning handle is alive. Clones copy the (≤ 32B) units rather than
    /// sharing an Arc.
    Small {
        len: u8,
        units: [u16; SMALL_STRING_CAP],
    },
    /// Lean append-node for `s += char` patterns. Each node stores a left
    /// and right pointer — like V8's ConsString — but no buffer. This
    /// makes each append a lean Gc allocation (~48 bytes). The accumulated
    /// string is materialized lazily on first `as_slice()` access via
    /// iterative traversal of the ConsString chain.
    ConsString {
        left: Handle<JsString>,
        right: Handle<JsString>,
        len: usize,
        depth: u32,
        flat: OnceLock<Arc<[u16]>>,
    },
    /// Full binary rope for non-trivial concatenations (both sides large, or
    /// when `ConsString` isn't the better fit). The binary-tree structure
    /// keeps depth logarithmic for balanced trees.
    Rope {
        left: Option<Handle<JsString>>,
        right: Option<Handle<JsString>>,
        len: usize,
        /// The rope's tree depth (0 for a flat leaf): concat folds the left
        /// side into one shared flat once it would exceed `ROPE_MAX_DEPTH`.
        /// Bounding the depth keeps the final drop (and any single flatten) at
        /// O(cap) nodes. u32 is far beyond any real append chain.
        depth: u32,
        /// The materialized contiguous form, computed on first access; immutable
        /// once set (strings are immutable), which is what makes `as_slice`
        /// return a stable reference. (Rope clones get a fresh cache — a rope
        /// flattened through two handles flattens twice.)
        flat: OnceLock<Arc<[u16]>>,
    },
}

/// A flattened-node worklist element: either a JsString node to process.
enum FlattenLeaf<'a> {
    String(&'a JsString),
}

/// Concatenations whose total length is at or below this stay flat: the rope
/// machinery (a node allocation plus a later flatten) only pays off once the
/// string is large enough that repeated copies would dominate.
const CONCAT_FLAT_THRESHOLD: usize = 16;

/// The inline-buffer capacity of the [`JsString::Small`] variant — the same
/// 16 units as `CONCAT_FLAT_THRESHOLD`, so every flat concat result and every
/// small literal fits the single-allocation form.
pub const SMALL_STRING_CAP: usize = 16;

impl Trace for JsString {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        match self {
            JsString::Flat(_) => {}
            JsString::Small { .. } => {}
            JsString::ConsString { left, right, .. } => {
                left.trace(visit);
                right.trace(visit);
            }
            JsString::Rope { left, right, .. } => {
                if let Some(l) = left {
                    l.trace(visit);
                }
                if let Some(r) = right {
                    r.trace(visit);
                }
            }
        }
    }
}

impl JsString {
    pub fn from_utf16(units: &[u16]) -> Self {
        // Cut 67: a small input's units live inline in the box — one
        // allocation instead of the Arc's own. Larger inputs keep the
        // Arc-backed flat (shared across clones).
        if units.len() <= SMALL_STRING_CAP {
            let mut buf = [0u16; SMALL_STRING_CAP];
            buf[..units.len()].copy_from_slice(units);
            return JsString::Small {
                len: units.len() as u8,
                units: buf,
            };
        }
        JsString::Flat(units.into())
    }

    pub fn from_utf8(text: &str) -> Self {
        let units: Vec<u16> = text.encode_utf16().collect();
        // Cut 67: small inputs copy into the inline box; larger ones keep the
        // Vec-to-Arc buffer reuse (`Arc::from(Vec)` moves the buffer, it does
        // not copy it).
        if units.len() <= SMALL_STRING_CAP {
            let mut buf = [0u16; SMALL_STRING_CAP];
            buf[..units.len()].copy_from_slice(&units);
            JsString::Small {
                len: units.len() as u8,
                units: buf,
            }
        } else {
            JsString::Flat(units.into())
        }
    }

    /// Concatenate without copying when the result is large enough: a rope
    /// node that IS the result box — one allocation per append. Small results
    /// (and empty operands) stay flat or reuse an operand. The operands' own
    /// boxes become the node's children (refcount bumps, no copies).
    ///
    /// For `s += 'x'` loops: Flat appends stay Flat up to CONCAT_FLAT_THRESHOLD.
    /// Beyond that, a lean ConsString node is created — just a left pointer
    /// and total length, no buffer/Vec/Arc. The accumulated string is
    /// materialized lazily on first `as_slice()` access.
    pub fn concat(left: &Handle<JsString>, right: &Handle<JsString>) -> Handle<JsString> {
        if right.is_empty() {
            return *left;
        }
        if left.is_empty() {
            return *right;
        }
        let len = left.len() + right.len();
        if len <= CONCAT_FLAT_THRESHOLD {
            // Cut 67: the result fits the inline buffer — copy both operand
            // slices into one `Small` box (a single allocation; the previous
            // `Flat` path paid a Vec + an Arc + the box for tiny strings).
            let mut buf = [0u16; SMALL_STRING_CAP];
            let l = left.as_slice();
            buf[..l.len()].copy_from_slice(l);
            let r = right.as_slice();
            buf[l.len()..len].copy_from_slice(r);
            return Handle::new(JsString::Small {
                len: len as u8,
                units: buf,
            });
        }
        // Both leaf (Flat or Small) → merge into Flat (17-128 units), or a
        // balanced Rope for two large Flats (Small operands cannot reach
        // len > 128). The in-place Rope construction writes straight into
        // the arena slot, skipping the stack-temp copy `Gc::new` pays for a
        // large enum.
        if left.is_leaf() && right.is_leaf() {
            if len <= 128 {
                let mut units = Vec::with_capacity(len);
                units.extend_from_slice(left.as_slice());
                units.extend_from_slice(right.as_slice());
                return Handle::new(JsString::Flat(units.into()));
            }
            return Handle::new_in_place(|ptr: *mut JsString| {
                // SAFETY: `new_in_place` hands back the fresh slot; this
                // closure writes every field before returning.
                unsafe {
                    ptr.write(JsString::Rope {
                        left: Some(*left),
                        right: Some(*right),
                        len,
                        depth: 1,
                        flat: OnceLock::new(),
                    });
                }
            });
        }
        // Default: create a lean ConsString append-node (like V8).
        // Each append = one Gc allocation, written in place. No buffer, no
        // Arc, no Vec clone.
        let depth = 1 + left.depth();
        Handle::new_in_place(|ptr: *mut JsString| {
            // SAFETY: `new_in_place` hands back the fresh slot; this
            // closure writes every field before returning.
            unsafe {
                ptr.write(JsString::ConsString {
                    left: *left,
                    right: *right,
                    len,
                    depth,
                    flat: OnceLock::new(),
                });
            }
        })
    }

    /// Whether the string is a leaf (`Flat` or `Small`): `as_slice` returns
    /// the units directly with no flatten cost.
    fn is_leaf(&self) -> bool {
        matches!(self, JsString::Flat(_) | JsString::Small { .. })
    }

    /// The tree depth: 0 for a flat string, else the node's cached depth.
    fn depth(&self) -> u32 {
        match self {
            JsString::Flat(_) | JsString::Small { .. } => 0,
            JsString::ConsString { depth, .. } | JsString::Rope { depth, .. } => *depth,
        }
    }

    /// The number of code units (spec 6.1.4.1 StringLength).
    pub fn len(&self) -> usize {
        match self {
            JsString::Flat(units) => units.len(),
            JsString::Small { len, .. } => *len as usize,
            JsString::ConsString { len, .. } | JsString::Rope { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_slice(&self) -> &[u16] {
        match self {
            JsString::Flat(units) => units,
            JsString::Small { len, units } => &units[..*len as usize],
            JsString::ConsString { flat, .. } | JsString::Rope { flat, .. } => {
                flat.get_or_init(|| self.flatten()).as_ref()
            }
        }
    }

    pub fn code_unit(&self, index: usize) -> Option<u16> {
        self.as_slice().get(index).copied()
    }

    /// spec 6.1.4.3 CodePointAt: `(code point, is unpaired surrogate, code units consumed)`.
    pub fn code_point_at(&self, index: usize) -> Option<(u32, bool, usize)> {
        let units = self.as_slice();
        let hi = *units.get(index)?;
        if (0xD800..=0xDBFF).contains(&hi) {
            if let Some(&lo) = units.get(index + 1)
                && (0xDC00..=0xDFFF).contains(&lo)
            {
                let cp = 0x10000 + (((hi as u32 - 0xD800) << 10) | (lo as u32 - 0xDC00));
                return Some((cp, false, 2));
            }
            Some((hi as u32, true, 1))
        } else if (0xDC00..=0xDFFF).contains(&hi) {
            Some((hi as u32, true, 1))
        } else {
            Some((hi as u32, false, 1))
        }
    }

    /// Iterate the code points (spec 6.1.4 StringToCodePoints), preserving
    /// lone surrogates as their own code point.
    pub fn code_points(&self) -> impl Iterator<Item = u32> + '_ {
        CodePointIter { s: self, pos: 0 }
    }

    pub fn to_code_points(&self) -> Vec<u32> {
        self.code_points().collect()
    }

    /// Lossy UTF-8 rendering for diagnostics; lone surrogates become U+FFFD.
    pub fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(self.as_slice())
    }

    /// The concatenation of the leaves, in order. Iterative (an explicit
    /// stack) so a deep left-leaning chain of small appends cannot overflow
    /// the call stack.
    fn flatten(&self) -> Arc<[u16]> {
        let mut units = Vec::with_capacity(self.len());
        self.flatten_into(&mut units);
        Arc::from(units)
    }

    /// Iterative leaf-order materialization of self into `units`.
    ///
    /// For ConsString, the tree is left-leaning: `left + right`. We traverse
    /// left-first by pushing the right child before left.
    fn flatten_into(&self, units: &mut Vec<u16>) {
        let mut stack: Vec<FlattenLeaf> = vec![FlattenLeaf::String(self)];
        while let Some(item) = stack.pop() {
            match item {
                FlattenLeaf::String(string) => match string {
                    JsString::Flat(flat) => units.extend_from_slice(flat.as_ref()),
                    JsString::Small { len, units: buf } => {
                        units.extend_from_slice(&buf[..*len as usize])
                    }
                    JsString::ConsString { left, right, .. } => {
                        // Push right first (so it's processed after left).
                        stack.push(FlattenLeaf::String(right));
                        stack.push(FlattenLeaf::String(left));
                    }
                    JsString::Rope { left, right, .. } => {
                        if let Some(r) = right {
                            stack.push(FlattenLeaf::String(r));
                        }
                        if let Some(l) = left {
                            stack.push(FlattenLeaf::String(l));
                        }
                    }
                },
            }
        }
    }
}

impl Clone for JsString {
    fn clone(&self) -> Self {
        match self {
            JsString::Flat(units) => JsString::Flat(units.clone()),
            JsString::Small { len, units } => JsString::Small {
                len: *len,
                units: *units,
            },
            JsString::ConsString {
                left,
                right,
                len,
                depth,
                ..
            } => JsString::ConsString {
                left: *left,
                right: *right,
                len: *len,
                depth: *depth,
                flat: OnceLock::new(),
            },
            JsString::Rope {
                left,
                right,
                len,
                depth,
                ..
            } => JsString::Rope {
                left: *left,
                right: *right,
                len: *len,
                depth: *depth,
                flat: OnceLock::new(),
            },
        }
    }
}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}
impl Eq for JsString {}

impl std::hash::Hash for JsString {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl fmt::Debug for JsString {
    /// Diagnostics show the text (not the unit numbers or the rope shape);
    /// the rope derive would recurse the tree and overflow on deep chains.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

struct CodePointIter<'a> {
    s: &'a JsString,
    pos: usize,
}

impl Iterator for CodePointIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let (cp, _, count) = self.s.code_point_at(self.pos)?;
        self.pos += count;
        Some(cp)
    }
}

impl fmt::Display for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

/// spec 6.1.4.5 CodePointsToString.
pub fn code_points_to_string(code_points: &[u32]) -> Result<JsString, crate::JsError> {
    let mut units = Vec::with_capacity(code_points.len());
    for &cp in code_points {
        if cp <= 0xFFFF {
            units.push(cp as u16);
        } else if cp <= 0x10FFFF {
            let x = cp - 0x10000;
            units.push(0xD800 + (x >> 10) as u16);
            units.push(0xDC00 + (x & 0x3FF) as u16);
        } else {
            return Err(crate::JsError::new(
                crate::ErrorKind::RangeError,
                "Invalid code point".into(),
            ));
        }
    }
    Ok(JsString::from_utf16(&units))
}

/// An interned string identifier; property keys use these for O(1) equality.
pub type AtomId = u32;

#[derive(Default)]
struct Interner {
    map: HashMap<Arc<[u16]>, AtomId>,
    atoms: Vec<Arc<[u16]>>,
}

impl Interner {
    fn intern(&mut self, units: &[u16]) -> AtomId {
        if let Some(&id) = self.map.get(units) {
            return id;
        }
        let id = self.atoms.len() as AtomId;
        let key: Arc<[u16]> = units.into();
        self.atoms.push(key.clone());
        self.map.insert(key, id);
        id
    }

    fn lookup(&self, id: AtomId) -> &[u16] {
        &self.atoms[id as usize]
    }
}

static INTERNER: OnceLock<Mutex<Interner>> = OnceLock::new();

fn with_interner<R>(f: impl FnOnce(&mut Interner) -> R) -> R {
    let mut guard = INTERNER
        .get_or_init(|| Mutex::new(Interner::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// The identifier hot path converts `AtomId`→`JsString` (lookup) and
/// `JsString`→`AtomId` (intern) several times per identifier read. Both take
/// the global interner lock and re-hash/copy the units every call; the
/// interpreter does this for the same handful of names in a loop, so a small
/// per-thread memo turns those into linear scans over a few cached entries.
/// The memo is a pure cache of the append-only interner: entries can never go
/// stale, so eviction is just a size cap.
const MEMO_CAP: usize = 64;

thread_local! {
    static LOOKUP_MEMO: std::cell::RefCell<Vec<(AtomId, JsString)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static INTERN_MEMO: std::cell::RefCell<Vec<(Arc<[u16]>, AtomId)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Interns `units`, returning a stable id for it.
pub fn intern(units: &[u16]) -> AtomId {
    INTERN_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some((_, id)) = memo.iter().rev().find(|(key, _)| key.as_ref() == units) {
            return *id;
        }
        let id = with_interner(|i| i.intern(units));
        memo.push((units.into(), id));
        if memo.len() > MEMO_CAP {
            memo.clear();
        }
        id
    })
}

pub fn intern_utf8(text: &str) -> AtomId {
    let units: Vec<u16> = text.encode_utf16().collect();
    intern(&units)
}

// The atom for the canonical decimal string of an array index, memoized per
// thread. The interner is a process-wide mutex and array hot loops (`a[i] =
// v` filling a dense array, arguments objects) re-intern the same handful of
// index strings over and over — each miss pays the lock. The interner is
// append-only, so the index → atom mapping is stable and the memo can never
// go stale.
thread_local! {
    static INDEX_ATOM_MEMO: std::cell::RefCell<std::collections::HashMap<u64, AtomId>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

// Bound the memo's memory (the distinct indices per program are usually a
// few hundred; the property-escape fixtures' chunked `buildString` reuses
// ~10k).
const INDEX_ATOM_MEMO_CAP: usize = 65536;

pub fn index_atom(index: u64) -> AtomId {
    INDEX_ATOM_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some(id) = memo.get(&index) {
            return *id;
        }
        let id = intern_utf8(&index.to_string());
        if memo.len() >= INDEX_ATOM_MEMO_CAP {
            memo.clear();
        }
        memo.insert(index, id);
        id
    })
}

/// The canonical atom for `"__proto__"` (the object-literal prototype
/// setter). Cached: the interner is a global (not thread-local), so the id
/// is process-stable, and the object-literal hot path compares against it
/// without paying `intern_utf8`'s per-call UTF-16 allocation.
pub fn proto_atom() -> AtomId {
    static PROTO: OnceLock<AtomId> = OnceLock::new();
    *PROTO.get_or_init(|| intern_utf8("__proto__"))
}

/// Returns the interned text for `id`.
pub fn lookup(id: AtomId) -> JsString {
    LOOKUP_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some((_, text)) = memo.iter().rev().find(|(cached, _)| *cached == id) {
            return text.clone();
        }
        let text = {
            let units = with_interner(|i| i.lookup(id).to_vec());
            JsString::from_utf16(&units)
        };
        memo.push((id, text.clone()));
        if memo.len() > MEMO_CAP {
            memo.clear();
        }
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_atom_matches_intern_and_is_stable() {
        // The memoized canonical-index atom must agree with the general
        // interner (a numeric key on the array's map and a string key from
        // elsewhere must collide) and stay stable across calls.
        for index in [0u64, 1, 9, 10, 99, 999_999, 0xFFFF_FFFE, 1 << 40] {
            let memoized = index_atom(index);
            assert_eq!(memoized, index_atom(index));
            assert_eq!(memoized, intern_utf8(&index.to_string()));
        }
        // The `length` key is not a canonical index string.
        assert_ne!(index_atom(0), intern_utf8("length"));
    }

    #[test]
    fn from_utf8_and_utf16_agree() {
        let a = JsString::from_utf8("hello");
        let b = JsString::from_utf16(&[
            b'h' as u16,
            b'e' as u16,
            b'l' as u16,
            b'l' as u16,
            b'o' as u16,
        ]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 5);
        assert!(!a.is_empty());
        assert!(JsString::from_utf8("").is_empty());
    }

    #[test]
    fn code_unit_indexing() {
        let s = JsString::from_utf8("abc");
        assert_eq!(s.code_unit(0), Some(b'a' as u16));
        assert_eq!(s.code_unit(3), None);
    }

    #[test]
    fn code_point_at_bmp() {
        let s = JsString::from_utf8("A");
        assert_eq!(s.code_point_at(0), Some((0x41, false, 1)));
        assert_eq!(s.code_point_at(1), None);
    }

    #[test]
    fn code_point_at_surrogate_pair() {
        // U+1F600 as a surrogate pair.
        let s = JsString::from_utf16(&[0xD83D, 0xDE00]);
        assert_eq!(s.code_point_at(0), Some((0x1F600, false, 2)));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn code_point_at_lone_surrogates() {
        let hi = JsString::from_utf16(&[0xD83D]);
        assert_eq!(hi.code_point_at(0), Some((0xD83D, true, 1)));
        let lo = JsString::from_utf16(&[0xDE00]);
        assert_eq!(lo.code_point_at(0), Some((0xDE00, true, 1)));
    }

    #[test]
    fn code_points_iterate_including_surrogates() {
        let s = JsString::from_utf16(&[b'a' as u16, 0xD83D, 0xDE00, 0xD800]);
        assert_eq!(s.to_code_points(), vec![0x61, 0x1F600, 0xD800]);
    }

    #[test]
    fn code_points_to_string_round_trips() {
        let s = JsString::from_utf16(&[b'a' as u16, 0xD83D, 0xDE00]);
        let cps = s.to_code_points();
        let back = code_points_to_string(&cps).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn code_points_to_string_rejects_out_of_range() {
        let err = code_points_to_string(&[0x110000]).unwrap_err();
        assert_eq!(err.kind, crate::ErrorKind::RangeError);
    }

    #[test]
    fn to_string_lossy_replaces_lone_surrogates() {
        let s = JsString::from_utf16(&[b'a' as u16, 0xD800]);
        assert_eq!(s.to_string_lossy(), "a\u{FFFD}");
        assert_eq!(s.to_string(), "a\u{FFFD}");
    }

    #[test]
    fn interning_is_stable_and_identity_based() {
        let a = intern_utf8("length");
        let b = intern_utf8("length");
        let c = intern_utf8("width");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(lookup(a).to_string_lossy(), "length");
        assert_eq!(lookup(c).to_string_lossy(), "width");
    }

    #[test]
    fn interning_preserves_utf16_content() {
        let s = JsString::from_utf16(&[0xD83D, 0xDE00]);
        let id = intern(s.as_slice());
        assert_eq!(lookup(id), s);
    }

    #[test]
    fn small_strings_are_the_inline_form_and_read_correctly() {
        // Cut 67: a small input's units live inline in the box — the read
        // paths (`len`/`as_slice`) serve them from the box, and a concat
        // whose result fits the inline buffer stays in the single-allocation
        // form.
        let s = JsString::from_utf8("hello");
        assert!(matches!(s, JsString::Small { .. }));
        assert_eq!(s.len(), 5);
        assert_eq!(
            s.as_slice(),
            &"hello".encode_utf16().collect::<Vec<u16>>()[..]
        );
        // The boundary: 16 units fit, 17 spill to the Arc-backed `Flat`.
        assert!(matches!(
            JsString::from_utf16(&[0x61; 16]),
            JsString::Small { .. }
        ));
        assert!(matches!(
            JsString::from_utf16(&[0x61; 17]),
            JsString::Flat(_)
        ));
        // A small concat result is Small too; a 17-unit result is Flat.
        let a = Handle::new(JsString::from_utf8("abcdefgh"));
        let b = Handle::new(JsString::from_utf8("ijklmnop"));
        let s = JsString::concat(&a, &b);
        assert!(matches!(*s, JsString::Small { .. }));
        assert_eq!(s.len(), 16);
        assert_eq!(
            s.as_slice(),
            &"abcdefghijklmnop".encode_utf16().collect::<Vec<u16>>()[..]
        );
        let c = Handle::new(JsString::from_utf8("q"));
        let s = JsString::concat(&s, &c);
        assert!(matches!(*s, JsString::Flat(_)));
        assert_eq!(s.len(), 17);
    }

    #[test]
    fn concat_small_stays_flat_and_equals_the_units() {
        let a = Handle::new(JsString::from_utf8("ab"));
        let b = Handle::new(JsString::from_utf8("cd"));
        let s = JsString::concat(&a, &b);
        // Cut 67: a small result is the single-allocation `Small` form (a
        // leaf, like `Flat`).
        assert!(matches!(*s, JsString::Flat(_) | JsString::Small { .. }));
        assert_eq!(s.len(), 4);
        assert_eq!(
            s.as_slice(),
            &[b'a' as u16, b'b' as u16, b'c' as u16, b'd' as u16]
        );
    }

    #[test]
    fn concat_large_builds_a_rope_with_correct_content() {
        // Cross the flat threshold by accumulating a long string.
        // Single-unit right operands produce `ConsString` (the append-heavy
        // fast path); two-unit chunks produce `Rope`.
        let mut s = Handle::new(JsString::from_utf8(""));
        let leaf = Handle::new(JsString::from_utf8("x"));
        for _ in 0..200 {
            s = JsString::concat(&s, &leaf);
        }
        // 64 single-unit appends → ConsString, not Rope.
        assert!(matches!(
            *s,
            JsString::ConsString { .. } | JsString::Rope { .. }
        ));
        assert_eq!(s.len(), 200);
        let units = s.as_slice();
        assert_eq!(units.len(), 200);
        assert!(units.iter().all(|&u| u == b'x' as u16));
        // The materialized form is cached: a second view is the same buffer.
        let again = s.as_slice();
        assert_eq!(units.as_ptr(), again.as_ptr());
    }

    #[test]
    fn rope_indexing_and_code_points() {
        let mut s = Handle::new(JsString::from_utf8("a"));
        let x = Handle::new(JsString::from_utf8("x"));
        for _ in 0..20 {
            s = JsString::concat(&s, &x);
        }
        let emoji = Handle::new(JsString::from_utf16(&[0xD83D, 0xDE00]));
        s = JsString::concat(&s, &emoji);
        assert_eq!(s.len(), 23); // 21 units + the surrogate pair
        assert_eq!(s.code_unit(0), Some(b'a' as u16));
        assert_eq!(s.code_unit(20), Some(b'x' as u16));
        assert_eq!(s.code_point_at(21), Some((0x1F600, false, 2)));
    }

    #[test]
    fn rope_equality_and_clone_correctness() {
        let mut left = Handle::new(JsString::from_utf8(""));
        let chunk = Handle::new(JsString::from_utf8("ab"));
        for _ in 0..32 {
            left = JsString::concat(&left, &chunk);
        }
        let mut right = Handle::new(JsString::from_utf8(""));
        for _ in 0..32 {
            right = JsString::concat(&right, &chunk);
        }
        assert_eq!(left.len(), 64);
        assert_eq!(left, right);
        assert_eq!(left.to_code_points(), right.to_code_points());
        // Clones share the rope's children (not its flat cache — each box has
        // its own), and materialize the same content.
        let cloned = left;
        assert_eq!(cloned.as_slice(), left.as_slice());
        assert_eq!(cloned.as_slice().len(), left.as_slice().len());
    }

    #[test]
    fn concat_with_empty_operand_reuses_the_other_side() {
        let s = Handle::new(JsString::from_utf8("hello"));
        let empty = Handle::new(JsString::from_utf8(""));
        assert_eq!(JsString::concat(&s, &empty).as_slice(), s.as_slice());
        assert_eq!(JsString::concat(&empty, &s).as_slice(), s.as_slice());
        assert_eq!(JsString::concat(&empty, &empty).len(), 0);
    }

    #[test]
    fn deep_append_chain_drops_and_flattens_iteratively() {
        // Left-leaning appends: the depth cap folds the tree every
        // ROPE_MAX_DEPTH appends, so it stays shallow — the fold's
        // materialization and the final drop exercise the iterative paths.
        let mut s = Handle::new(JsString::from_utf8(""));
        let leaf = Handle::new(JsString::from_utf8("x"));
        for i in 0..200_000 {
            s = JsString::concat(&s, &leaf);
            if !(3..199_998).contains(&i) {
                eprintln!("iter {}: len={}", i, s.len());
            }
        }
        let total = s.as_slice().len();
        eprintln!("total_len={}", total);
        assert_eq!(total, 200_000);
        let units = s.as_slice();
        assert!(units.iter().all(|&u| u == b'x' as u16));

        // Right-leaning prepends: the cap only inspects the left side, so the
        // depth grows unbounded — drop and flatten must still be iterative.
        let mut p = Handle::new(JsString::from_utf8(""));
        for _ in 0..200_000 {
            p = JsString::concat(&leaf, &p);
        }
        let plen = p.len();
        let punits = p.as_slice();
        eprintln!("prepend: len={} as_slice_len={}", plen, punits.len());
        assert_eq!(plen, 200_000);
        let units = p.as_slice();
        assert_eq!(units.len(), 200_000);
        assert!(units.iter().all(|&u| u == b'x' as u16));
    }

    #[test]
    fn cons_string_flattens_correctly() {
        // Verify that nested ConsString (single-unit right operands) flatten
        // in left-to-right order, not reversed.
        let mut s = Handle::new(JsString::from_utf16(&[b'a' as u16]));
        let x = Handle::new(JsString::from_utf8("x"));
        for _ in 0..20 {
            s = JsString::concat(&s, &x);
        }
        eprintln!(
            "after 20 appends: len={} starts_with_a={}",
            s.len(),
            s.code_unit(0) == Some(b'a' as u16)
        );
        let emoji = Handle::new(JsString::from_utf16(&[0xD83D, 0xDE00]));
        s = JsString::concat(&s, &emoji);
        eprintln!(
            "after emoji: len={} slice_len={}",
            s.len(),
            s.as_slice().len()
        );
        assert_eq!(s.len(), 23); // 21 units + the surrogate pair
        assert_eq!(s.code_unit(0), Some(b'a' as u16));
        assert_eq!(s.code_unit(20), Some(b'x' as u16));
        assert_eq!(s.code_point_at(21), Some((0x1F600, false, 2)));
        // Content is correct (Flat after emoji merge).
        assert_eq!(s.as_slice().len(), 23);
    }
}
